use psa::api::{self as api, ClientError};
use psa_server::probes::ProbeRegistry;
use psa_store::Database;
use tokio::{net::TcpListener, sync::oneshot};

#[path = "support/fake_probe.rs"]
mod fake_probe;
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

fn assert_api_error(error: ClientError, expected_status: u16, expected_code: &str) {
    match error {
        ClientError::Api { status, error } => {
            assert_eq!(status, expected_status);
            assert_eq!(error.code, expected_code);
        }
        other => panic!("expected API error, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_listing_is_authenticated_read_only_and_uses_connected_probe() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let client = api::Client::new(&base, "test-client-key").unwrap();
    let probes = ProbeRegistry::start(fake_probe::PROBE_KEY.into());
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(psa_server::server::serve_with_configuration(
        listener,
        database,
        Default::default(),
        probes.clone(),
        psa_server::config_registry::ConfigRegistry::new(directory.path().join("config")),
        psa_server::config::default_model_config(),
        "test-client-key".into(),
        async {
            let _ = stopped.await;
        },
    ));
    let probe = fake_probe::connect_with_files(
        address,
        &probes,
        fake_probe::FakeFiles::default()
            .directory("/a space & ü/child")
            .file("/a space & ü/file.txt", "abc")
            .symlink("/a space & ü/link", "target")
            .list_error("/denied"),
    )
    .await;
    let id = support::probe_metadata().id;
    let entries = client
        .list_probe_directory(id, "/a space & ü")
        .await
        .unwrap();
    assert_eq!(entries.len(), 3);
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "child" && entry.kind == "directory")
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "file.txt" && entry.kind == "file" && entry.size == 3)
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "link" && entry.kind == "symlink")
    );
    assert!(
        client
            .list_probe_directory(id, "/a space & ü/child")
            .await
            .unwrap()
            .is_empty()
    );
    assert_api_error(
        client
            .list_probe_directory(id, "relative")
            .await
            .unwrap_err(),
        400,
        "invalid_input",
    );
    assert_api_error(
        client
            .list_probe_directory(id, "/bad\0path")
            .await
            .unwrap_err(),
        400,
        "invalid_input",
    );
    assert_api_error(
        client
            .list_probe_directory(id, "/denied")
            .await
            .unwrap_err(),
        502,
        "probe_error",
    );
    let unknown = api::ProbeId(api::probes::Uuid::new_v4());
    assert_api_error(
        client.list_probe_directory(unknown, "/").await.unwrap_err(),
        404,
        "not_found",
    );
    let response = reqwest::Client::new()
        .get(format!("{base}/probes/{id}/directories?path=%2F"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = reqwest::Client::new()
        .post(format!("{base}/probes/{id}/directories?path=%2F"))
        .bearer_auth("test-client-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    probe.disconnect(&probes).await;
    assert_api_error(
        client.list_probe_directory(id, "/").await.unwrap_err(),
        503,
        "unavailable",
    );
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
