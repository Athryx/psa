use psa_probe::config::StartOptions;

#[tokio::test]
async fn uuid_persists_and_credentials_are_required() {
    let directory = tempfile::tempdir().unwrap();
    let options = || StartOptions {
        data_dir: Some(directory.path().to_owned()),
        server: Some("http://localhost:8080".into()),
        ..Default::default()
    };
    assert!(options().resolve().await.is_err());
    let initial = tokio::fs::read_to_string(directory.path().join("probe.toml"))
        .await
        .unwrap();
    tokio::fs::write(
        directory.path().join("auth.toml"),
        "[psa]\nprobe_key='test'\n[openrouter]\napi_key='unused'\n",
    )
    .await
    .unwrap();
    let first = options().resolve().await.unwrap();
    let second = StartOptions {
        data_dir: Some(directory.path().to_owned()),
        ..Default::default()
    }
    .resolve()
    .await
    .unwrap();
    assert_eq!(first.metadata.id, second.metadata.id);
    assert!(!first.metadata.host);
    assert!(!second.metadata.host);
    assert_eq!(first.server_url, "ws://localhost:8080/probes/connect");
    assert_eq!(
        initial,
        tokio::fs::read_to_string(directory.path().join("probe.toml"))
            .await
            .unwrap()
    );
    tokio::fs::write(directory.path().join("probe.toml"), "id='not-a-uuid'")
        .await
        .unwrap();
    assert!(options().resolve().await.is_err());
    assert_eq!(
        tokio::fs::read_to_string(directory.path().join("probe.toml"))
            .await
            .unwrap(),
        "id='not-a-uuid'"
    );
}

#[tokio::test]
async fn missing_explicit_config_is_not_created() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing.toml");
    assert!(
        StartOptions {
            data_dir: Some(directory.path().to_owned()),
            config: Some(path.clone()),
            ..Default::default()
        }
        .resolve()
        .await
        .is_err()
    );
    assert!(!tokio::fs::try_exists(&path).await.unwrap());
}

#[tokio::test]
async fn host_marker_defaults_false_and_reads_only_root_server_field() {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::write(
        directory.path().join("auth.toml"),
        "[psa]\nprobe_key='test'\n",
    )
    .await
    .unwrap();
    let options = || StartOptions {
        data_dir: Some(directory.path().to_owned()),
        server: Some("http://localhost:8080".into()),
        ..Default::default()
    };
    assert!(!options().resolve().await.unwrap().metadata.host);

    let path = directory.path().join("config.toml");
    assert!(!tokio::fs::try_exists(&path).await.unwrap());

    for (text, host) in [
        ("", false),
        ("[server]\nhost = true\nlisten = 42\n", false),
        ("host = true\n[server]\nunknown = 'ignored'\n", true),
        ("host = false\nserver = 'not a server table'\n", false),
    ] {
        tokio::fs::write(&path, text).await.unwrap();
        assert_eq!(options().resolve().await.unwrap().metadata.host, host);
        // Reading a legacy config never inserts a host marker or rewrites it.
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), text);
    }

    for text in [
        "host = 'true'",
        "host = 1",
        "host = []",
        "host = { enabled = true }",
        "broken TOML [",
    ] {
        tokio::fs::write(&path, text).await.unwrap();
        assert!(options().resolve().await.is_err(), "accepted {text}");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), text);
    }
}

#[tokio::test]
async fn host_marker_is_beside_selected_probe_config_not_data_directory() {
    let directory = tempfile::tempdir().unwrap();
    let selected = tempfile::tempdir().unwrap();
    tokio::fs::write(directory.path().join("config.toml"), "host = true\n")
        .await
        .unwrap();
    tokio::fs::write(
        selected.path().join("auth.toml"),
        "[psa]\nprobe_key='test'\n",
    )
    .await
    .unwrap();
    let probe_path = selected.path().join("custom-probe.toml");
    tokio::fs::write(&probe_path, "server_url = 'http://localhost:8080'\n")
        .await
        .unwrap();
    let options = || StartOptions {
        data_dir: Some(directory.path().to_owned()),
        config: Some(probe_path.clone()),
        ..Default::default()
    };
    assert!(!options().resolve().await.unwrap().metadata.host);

    let host_path = selected.path().join("config.toml");
    for host in [true, false] {
        tokio::fs::write(&host_path, format!("host = {host}\n"))
            .await
            .unwrap();
        assert_eq!(options().resolve().await.unwrap().metadata.host, host);
    }

    // The marker is not a probe.toml field.
    tokio::fs::write(
        &probe_path,
        "host = true\nserver_url = 'http://localhost:8080'\n",
    )
    .await
    .unwrap();
    assert!(options().resolve().await.is_err());
}
