use psa::api::{self as api, ClientError, CreateSandbox, Mount, SandboxState};
use psa_server::{db::Database, probes::ProbeRegistry};
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

fn request(image: Option<&str>, name: &str) -> CreateSandbox {
    CreateSandbox {
        image: image.map(Into::into),
        name: Some(name.into()),
        mounts: vec![Mount {
            host_path: "/src".into(),
            container_path: "/work".into(),
            writable: true,
        }],
        start: true,
    }
}

#[tokio::test]
async fn sandboxes_follow_manager_lifecycle_and_report_errors() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = api::Client::new(&format!("http://{address}"), "test-client-key").unwrap();
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
    let manager = support::probe_metadata().id;

    // A probe without sandbox support rejects creation.
    let probe =
        fake_probe::FakeProbe::connect_with_files(address, &probes, Default::default()).await;
    assert_api_error(
        client
            .create_sandbox(manager, &request(Some("img"), "a"))
            .await
            .unwrap_err(),
        400,
        "sandboxes_unsupported",
    );
    probe.disconnect(&probes).await;

    let files = fake_probe::FakeFiles::default().with_sandboxes();
    let probe = fake_probe::FakeProbe::connect_with_files(address, &probes, files).await;
    assert!(client.get_probe(manager).await.unwrap().metadata.sandboxes);

    // This server has no default image.
    assert_api_error(
        client
            .create_sandbox(manager, &request(None, "a"))
            .await
            .unwrap_err(),
        400,
        "image_required",
    );
    assert_api_error(
        client
            .create_sandbox(manager, &request(Some("img"), "bad name"))
            .await
            .unwrap_err(),
        400,
        "invalid_input",
    );

    let sandbox = client
        .create_sandbox(manager, &request(Some("img"), "a"))
        .await
        .unwrap();
    assert_eq!(sandbox.manager_id, manager);
    assert_eq!(sandbox.state, SandboxState::Running);
    assert!(!sandbox.probe_active);
    assert_api_error(
        client
            .create_sandbox(manager, &request(Some("img"), "a"))
            .await
            .unwrap_err(),
        400,
        "invalid_input",
    );

    let stopped_sandbox = client.stop_sandbox(sandbox.probe_id).await.unwrap();
    assert_eq!(stopped_sandbox.state, SandboxState::Stopped);
    let listed = client.list_sandboxes(Some(manager)).await.unwrap();
    assert_eq!(listed, vec![stopped_sandbox.clone()]);

    // Without the manager the state is unknown and lifecycle changes fail.
    probe.disconnect(&probes).await;
    let offline = client.get_sandbox(sandbox.probe_id).await.unwrap();
    assert_eq!(offline.state, SandboxState::Unknown);
    assert_api_error(
        client.start_sandbox(sandbox.probe_id).await.unwrap_err(),
        409,
        "manager_offline",
    );
    assert_api_error(
        client.delete_sandbox(sandbox.probe_id).await.unwrap_err(),
        409,
        "manager_offline",
    );

    // A reconnected manager with no container reports it missing; delete keeps the record.
    let files = fake_probe::FakeFiles::default().with_sandboxes();
    let probe = fake_probe::FakeProbe::connect_with_files(address, &probes, files).await;
    let missing = client.get_sandbox(sandbox.probe_id).await.unwrap();
    assert_eq!(missing.state, SandboxState::Missing);
    client.delete_sandbox(sandbox.probe_id).await.unwrap();
    let listed = client.list_sandboxes(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, SandboxState::Deleted);
    assert_eq!(listed[0].manager_id, sandbox.manager_id);
    let deleted = client.get_sandbox(sandbox.probe_id).await.unwrap();
    assert_eq!(deleted.state, SandboxState::Deleted);
    for result in [
        client.start_sandbox(sandbox.probe_id).await.map(|_| ()),
        client.delete_sandbox(sandbox.probe_id).await,
    ] {
        assert_api_error(result.unwrap_err(), 409, "sandbox_deleted");
    }

    probe.disconnect(&probes).await;
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn contexts_restricted_to_a_manager_list_and_manage_its_sandboxes() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("test.sqlite3"))
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = api::Client::new(&format!("http://{address}"), "test-client-key").unwrap();
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
    let files = fake_probe::FakeFiles::default().with_sandboxes();
    let probe = fake_probe::FakeProbe::connect_with_files(address, &probes, files).await;
    let manager = support::probe_metadata().id;

    let context = psa::Context::builder(client.clone(), "sandboxed")
        .probe(psa::ProbeSelector::Id(manager))
        .build()
        .await
        .unwrap();
    assert_eq!(context.probe_restriction(), Some(manager));

    // The new sandbox's probe has not connected, so it has no probe record yet.
    let mut sandbox = context
        .create_sandbox(&request(Some("img"), "worker"))
        .await
        .unwrap();
    assert!(sandbox.is_sandbox() && !sandbox.is_live());
    assert!(sandbox.info().is_none());
    let found = context
        .find_probe(psa::ProbeSelector::Sandbox("worker".into()))
        .await
        .unwrap();
    assert_eq!(found.id(), sandbox.id());
    assert_eq!(context.probes().await.unwrap().len(), 2);

    sandbox.stop().await.unwrap();
    assert_eq!(sandbox.sandbox().unwrap().state, SandboxState::Stopped);

    // Deleted sandboxes are hidden from listings and cannot be brought up.
    client.delete_sandbox(sandbox.id()).await.unwrap();
    assert_eq!(context.probes().await.unwrap().len(), 1);
    let mut deleted = context.probe(sandbox.id()).await.unwrap();
    assert!(matches!(
        deleted.ensure_up().await,
        Err(psa::Error::SandboxUnavailable {
            state: SandboxState::Deleted,
            ..
        })
    ));

    probe.disconnect(&probes).await;
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
