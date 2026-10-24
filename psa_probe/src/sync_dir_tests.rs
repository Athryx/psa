use super::*;

fn archive() -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(3);
    header.set_mode(0o751);
    header.set_cksum();
    archive
        .append_data(&mut header, "nested/new", &b"new"[..])
        .unwrap();
    archive.into_inner().unwrap()
}

#[test]
fn publication_lock_serializes_parent_aliases() {
    let parent = tempfile::tempdir().unwrap();
    let real = parent.path().join("real");
    let alias = parent.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let guard = lock_directory_publication(&real.join("target")).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _guard = lock_directory_publication(&alias.join("target")).unwrap();
        sender.send(()).unwrap();
    });
    assert!(
        receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
    );
    drop(guard);
    receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    thread.join().unwrap();
}

#[test]
fn concurrent_syncs_can_all_publish_to_initially_missing_destination() {
    let parent = tempfile::tempdir().unwrap();
    let destination = parent.path().join("target");
    let barrier = Arc::new(std::sync::Barrier::new(8));
    std::thread::scope(|threads| {
        for _ in 0..8 {
            let barrier = barrier.clone();
            let destination = &destination;
            threads.spawn(move || {
                barrier.wait();
                extract_directory_archive(
                    destination,
                    &archive()[..],
                    &AtomicBool::new(false),
                    true,
                )
                .unwrap();
            });
        }
    });
    assert_eq!(
        std::fs::read(destination.join("nested/new")).unwrap(),
        b"new"
    );
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
}

#[test]
fn sync_operation_uses_generic_path_wire_format() {
    let operation = Operation::SyncDir {
        path: PathBuf::from("/cache/skills"),
    };
    let value = serde_json::to_value(operation).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"op": "sync_dir", "path": "/cache/skills"})
    );
    assert!(
        matches!(serde_json::from_value::<Operation>(value).unwrap(), Operation::SyncDir { path } if path == Path::new("/cache/skills"))
    );
}

#[test]
fn sync_creates_then_replaces_entire_directory_and_cleans_old_tree() {
    let parent = tempfile::tempdir().unwrap();
    let destination = parent.path().join("destination");
    let cancelled = AtomicBool::new(false);
    extract_directory_archive(&destination, &archive()[..], &cancelled, true).unwrap();
    assert_eq!(
        std::fs::read(destination.join("nested/new")).unwrap(),
        b"new"
    );
    assert_eq!(
        std::fs::metadata(destination.join("nested/new"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    std::fs::write(destination.join("stale"), b"old").unwrap();
    let outside = parent.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, destination.join("link")).unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::set_permissions(
        destination.join("nested"),
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o500)).unwrap();

    extract_directory_archive(&destination, &archive()[..], &cancelled, true).unwrap();
    assert!(!destination.join("stale").exists());
    assert!(!destination.join("link").exists());
    assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep");
    assert_eq!(
        std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o500
    );
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 2);
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn sync_rejects_file_and_symlink_destinations() {
    let parent = tempfile::tempdir().unwrap();
    let file = parent.path().join("file");
    std::fs::write(&file, b"keep").unwrap();
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let dangling = parent.path().join("dangling");
    std::os::unix::fs::symlink(parent.path().join("missing"), &dangling).unwrap();
    for destination in [&file, &link, &dangling] {
        assert!(
            extract_directory_archive(destination, &archive()[..], &AtomicBool::new(false), true)
                .is_err()
        );
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    assert!(std::fs::symlink_metadata(&dangling).unwrap().is_symlink());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 3);
}

#[test]
fn failed_and_cancelled_sync_preserve_destination_and_clean_staging() {
    let parent = tempfile::tempdir().unwrap();
    let destination = parent.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("keep"), b"keep").unwrap();
    assert!(
        extract_directory_archive(
            &destination,
            &b"invalid tar"[..],
            &AtomicBool::new(false),
            true
        )
        .is_err()
    );
    assert!(
        extract_directory_archive(&destination, &archive()[..], &AtomicBool::new(true), true)
            .is_err()
    );
    assert_eq!(std::fs::read(destination.join("keep")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
}

#[test]
fn upload_still_refuses_existing_directory() {
    let parent = tempfile::tempdir().unwrap();
    let destination = parent.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("keep"), b"keep").unwrap();
    assert!(
        extract_directory_archive(&destination, &archive()[..], &AtomicBool::new(false), false)
            .is_err()
    );
    assert_eq!(std::fs::read(destination.join("keep")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
}
