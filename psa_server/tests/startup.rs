mod support;
use clap::Parser;
use psa_server::{
    config::{Cli, Command, ConfigError, StartOptions},
    db::Database,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[test]
fn clap_parses_start_options_and_rejects_bad_listen_addresses() {
    let cli = Cli::try_parse_from([
        "psa_server",
        "start",
        "--data-dir",
        "/tmp/test",
        "--listen",
        "127.0.0.1:0",
    ])
    .unwrap();
    let Command::Start(options) = cli.command;
    assert_eq!(options.listen.unwrap().port(), 0);
    assert!(Cli::try_parse_from(["psa_server", "start", "--listen", "invalid"]).is_err());
}

#[tokio::test]
async fn config_uses_data_directory_and_never_overwrites_invalid_or_explicit_files() {
    let dir = tempfile::tempdir().unwrap();
    let options = || StartOptions {
        data_dir: Some(dir.path().into()),
        ..Default::default()
    };
    let config = options().resolve().await.unwrap();
    assert_eq!(config.database_path, dir.path().join("psa.sqlite3"));
    assert!(config.config_path.exists());
    tokio::fs::write(&config.config_path, "broken TOML [")
        .await
        .unwrap();
    assert!(matches!(
        options().resolve().await,
        Err(ConfigError::Parse { .. })
    ));
    assert_eq!(
        tokio::fs::read_to_string(&config.config_path)
            .await
            .unwrap(),
        "broken TOML ["
    );
    let missing = dir.path().join("missing.toml");
    assert!(matches!(
        (StartOptions {
            config: Some(missing.clone()),
            ..options()
        })
        .resolve()
        .await,
        Err(ConfigError::Io { .. })
    ));
    assert!(!missing.exists());
    let custom = dir.path().join("custom.toml");
    tokio::fs::write(
        &custom,
        "[server]\nlisten = \"127.0.0.1:1111\"\n[database]\npath = \"custom.sqlite3\"\n",
    )
    .await
    .unwrap();
    let resolved = StartOptions {
        config: Some(custom),
        listen: Some("127.0.0.1:2222".parse().unwrap()),
        ..options()
    }
    .resolve()
    .await
    .unwrap();
    assert_eq!(resolved.listen.port(), 2222);
    assert_eq!(resolved.database_path, dir.path().join("custom.sqlite3"));
}

#[tokio::test]
async fn migrated_server_serves_health_and_shuts_down_cleanly() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("server.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&database).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(psa_server::server::serve_with_providers(
            listener,
            database,
            psa_server::providers::ProviderRegistry::default().with_catalog_source(
                psa_server::models::CatalogSource::new(&format!("http://{address}")).unwrap(),
            ),
            async {
                let _ = wait.await;
            },
        ));
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("ok\n"));
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        let reopened = Database::open(dir.path().join("server.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&reopened).await;
        reopened.close().await;
    })
    .await
    .unwrap();
}
