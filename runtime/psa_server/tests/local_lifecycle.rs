#![cfg(unix)]

use client_shared::local::{LocalServerManager, LocalServerStatus};

#[tokio::test]
async fn local_server_survives_manager_and_supports_restart_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("config.toml"), "[server]\nlisten = '0.0.0.0:1234'\nautostart_local_probe = false\n[database]\npath = 'psa.sqlite3'\n").await.unwrap();
    let manager =
        || LocalServerManager::new(env!("CARGO_BIN_EXE_psa_server").into(), dir.path().into());
    assert!(matches!(
        manager().status().await.unwrap(),
        LocalServerStatus::Stopped
    ));
    let LocalServerStatus::Running { connection } = manager().start().await.unwrap() else {
        panic!("not running")
    };
    assert!(!connection.client_key.is_empty());
    assert!(connection.api_url.starts_with("http://127.0.0.1:"));
    assert_ne!(connection.api_url, "http://127.0.0.1:1234");
    let duplicate = tokio::process::Command::new(env!("CARGO_BIN_EXE_psa_server"))
        .arg("start")
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--listen")
        .arg("127.0.0.1:0")
        .output()
        .await
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(
        reqwest::Client::new()
            .get(format!("{}/health", connection.api_url))
            .bearer_auth(&connection.client_key)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let LocalServerStatus::Running {
        connection: observed,
    } = manager().status().await.unwrap()
    else {
        panic!("not running")
    };
    assert_eq!(connection.api_url, observed.api_url);
    assert_eq!(connection.client_key, observed.client_key);
    assert!(matches!(
        manager().restart().await.unwrap(),
        LocalServerStatus::Running { .. }
    ));
    assert!(matches!(
        manager().stop().await.unwrap(),
        LocalServerStatus::Stopped
    ));
    assert!(matches!(
        manager().stop().await.unwrap(),
        LocalServerStatus::Stopped
    ));
}
