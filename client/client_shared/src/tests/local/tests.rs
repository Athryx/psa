use super::*;
use std::os::unix::fs::PermissionsExt;

async fn write_script(path: &Path, body: &str) {
    tokio::fs::write(path, format!("#!/bin/sh\n{body}\n"))
        .await
        .unwrap();
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_startup_reports_arguments_and_log_tail() {
    let directory = tempfile::tempdir().unwrap();
    let server = directory.path().join("fake_server");
    write_script(&server, r#"echo "args: $*"; echo "bad config" >&2; exit 3"#).await;
    let data_dir = directory.path().join("data");
    let manager = LocalServerManager::new(server, data_dir.clone())
        .with_listen(Some("127.0.0.1:0".parse().unwrap()))
        .with_probe_binary(Some("/opt/psa_probe".into()));

    let error = manager.start().await.unwrap_err().to_string();

    assert!(error.contains("exited during startup"), "{error}");
    assert!(error.contains("bad config"), "{error}");
    assert!(
        error.contains(&format!(
            "args: start --data-dir {} --listen 127.0.0.1:0 --probe-binary /opt/psa_probe",
            data_dir.display()
        )),
        "{error}"
    );
    let mode = tokio::fs::metadata(manager.log_path())
        .await
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn startup_truncates_previous_log() {
    let directory = tempfile::tempdir().unwrap();
    let server = directory.path().join("fake_server");
    write_script(&server, "echo second run; exit 1").await;
    let manager = LocalServerManager::new(server, directory.path().join("data"));
    tokio::fs::create_dir_all(manager.log_path().parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(manager.log_path(), "first run\n")
        .await
        .unwrap();

    assert!(manager.start().await.is_err());

    let log = tokio::fs::read_to_string(manager.log_path()).await.unwrap();
    assert_eq!(log, "second run\n");
}

#[tokio::test]
async fn missing_executable_names_its_path() {
    let directory = tempfile::tempdir().unwrap();
    let server = directory.path().join("missing_server");
    let manager = LocalServerManager::new(server.clone(), directory.path().join("data"));

    let error = manager.start().await.unwrap_err().to_string();
    assert!(error.contains(&server.display().to_string()), "{error}");
}
