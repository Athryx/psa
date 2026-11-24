use super::*;
use std::os::unix::fs::PermissionsExt;

const LIMITS: Limits = Limits {
    max_unpacked_bytes: 1 << 20,
    max_entries: 100,
};

fn plain_archive(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Archive {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    Archive {
        tar_data: builder.into_inner().unwrap(),
        compression: ArchiveCompression::None,
    }
}

fn append(builder: &mut tar::Builder<Vec<u8>>, kind: tar::EntryType, path: &str, data: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    if kind == tar::EntryType::Symlink {
        header.set_link_name("/etc/passwd").unwrap();
    }
    // `append_data` rejects `..`, so write the name into the header directly.
    header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

async fn read(archive: &Archive) -> Result<Entries, ArchiveError> {
    read_entries(archive, LIMITS).await
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn rejects_links_escapes_duplicates_and_oversized_archives() {
    let symlink = plain_archive(|tar| append(tar, tar::EntryType::Symlink, "link", b""));
    let escape = plain_archive(|tar| append(tar, tar::EntryType::Regular, "../x", b"x"));
    let duplicate = plain_archive(|tar| {
        append(tar, tar::EntryType::Regular, "a", b"1");
        append(tar, tar::EntryType::Regular, "./a", b"2");
    });
    for archive in [symlink, escape, duplicate] {
        assert!(matches!(
            read(&archive).await,
            Err(ArchiveError::Invalid(_))
        ));
    }

    let big = Archive::builder()
        .file("big", vec![0; 2 << 20])
        .build()
        .unwrap();
    assert!(matches!(read(&big).await, Err(ArchiveError::TooLarge(_))));
}

#[tokio::test]
async fn extracts_files_and_applies_modes_after_writing() {
    let root = tempfile::tempdir().unwrap();
    let archive = Archive::builder()
        .file("src/lib.rs", "code")
        .executable("run.sh", "#!/bin/sh")
        .dir("empty")
        .build()
        .unwrap();
    let mut entries = read(&archive).await.unwrap();
    // A read-only directory must not block the files inside it.
    let mut list = entries.entries().to_vec();
    list.push(Entry {
        path: "locked".into(),
        directory: true,
        mode: 0o555,
        contents: 0..0,
    });
    list.push(Entry {
        path: "locked/inner".into(),
        directory: false,
        mode: 0o444,
        contents: list[0].contents.clone(),
    });
    entries.entries = list.into();

    let extracted = extract_into(&entries, root.path()).await.unwrap();

    assert_eq!(
        std::fs::read(root.path().join("src/lib.rs")).unwrap(),
        b"code"
    );
    assert_eq!(mode(&root.path().join("src")), 0o755);
    assert_eq!(mode(&root.path().join("run.sh")), 0o755);
    assert_eq!(mode(&root.path().join("src/lib.rs")), 0o644);
    assert_eq!(mode(&root.path().join("locked")), 0o555);
    assert_eq!(mode(&root.path().join("locked/inner")), 0o444);
    assert!(root.path().join("empty").is_dir());

    extracted.remove().await;
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn merges_existing_directories_and_rolls_back_on_conflict() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::set_permissions(
        root.path().join("src"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    std::fs::write(root.path().join("src/old.rs"), "old").unwrap();

    let merged = Archive::builder()
        .dir("src")
        .file("src/new.rs", "new")
        .build()
        .unwrap();
    let extracted = extract_into(&read(&merged).await.unwrap(), root.path())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(root.path().join("src/new.rs")).unwrap(),
        b"new"
    );
    // Directories that already existed keep their mode.
    assert_eq!(mode(&root.path().join("src")), 0o750);
    extracted.remove().await;
    assert!(!root.path().join("src/new.rs").exists());
    assert!(root.path().join("src/old.rs").exists());

    let conflicting = Archive::builder()
        .file("added/a.txt", "a")
        .file("src/old.rs", "replaced")
        .build()
        .unwrap();
    let error = extract_into(&read(&conflicting).await.unwrap(), root.path())
        .await
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Invalid(message) if message.contains("already exists")));
    assert_eq!(
        std::fs::read(root.path().join("src/old.rs")).unwrap(),
        b"old"
    );
    assert!(!root.path().join("added").exists());
}

#[tokio::test]
async fn never_follows_symlinks_under_the_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("target"), root.path().join("file")).unwrap();

    for path in ["link/escaped", "file"] {
        let archive = Archive::builder().file(path, "x").build().unwrap();
        assert!(
            extract_into(&read(&archive).await.unwrap(), root.path())
                .await
                .is_err(),
            "{path}"
        );
    }
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}
