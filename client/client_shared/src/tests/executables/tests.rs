use super::*;

async fn write_file(path: &Path, mode: u32) {
    tokio::fs::write(path, b"#!/bin/sh\n").await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn explicit_path_is_used_unchanged() {
    let path = PathBuf::from("relative/psa_server");
    let resolved = resolve_executable(Executable::Server, Some(path.clone()))
        .await
        .unwrap();
    assert_eq!(resolved, path);
}

#[tokio::test]
async fn path_search_takes_first_executable_match() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_file(&second.path().join("psa_probe"), 0o755).await;
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();

    let found = find_on_path("psa_probe", Some(path)).await;
    assert_eq!(found, Some(second.path().join("psa_probe")));
}

#[cfg(unix)]
#[tokio::test]
async fn path_search_skips_non_executable_files_and_directories() {
    let plain = tempfile::tempdir().unwrap();
    let nested = tempfile::tempdir().unwrap();
    let executable = tempfile::tempdir().unwrap();
    write_file(&plain.path().join("psa_server"), 0o644).await;
    tokio::fs::create_dir(nested.path().join("psa_server"))
        .await
        .unwrap();
    write_file(&executable.path().join("psa_server"), 0o700).await;
    let path = std::env::join_paths([plain.path(), nested.path(), executable.path()]).unwrap();

    let found = find_on_path("psa_server", Some(path)).await;
    assert_eq!(found, Some(executable.path().join("psa_server")));
}

#[tokio::test]
async fn path_search_without_match_finds_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert_eq!(find_on_path("psa_desktop", Some(path)).await, None);
    assert_eq!(find_on_path("psa_desktop", None).await, None);
}
