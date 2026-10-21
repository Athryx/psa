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
    assert!(!path.exists());
}
