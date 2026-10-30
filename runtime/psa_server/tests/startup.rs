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
    assert_eq!(
        config.session_defaults,
        psa_server::config::default_model_config()
    );
    assert_eq!(
        config.session_defaults.model.provider,
        llm_api::providers::ProviderType::OpenAiCodex
    );
    assert_eq!(config.session_defaults.model.api_name, "gpt-5");
    assert_eq!(
        config.session_defaults.effort,
        llm_api::providers::Effort::Medium
    );
    let generated = tokio::fs::read_to_string(&config.config_path)
        .await
        .unwrap();
    assert!(config.host);
    assert_eq!(config.default_agent_name, "default");
    assert_eq!(
        toml::from_str::<toml::Value>(&generated).unwrap()["host"].as_bool(),
        Some(true)
    );
    assert!(generated.contains("[session_defaults]"));
    assert!(generated.contains("model = \"openai_codex/gpt-5\""));
    assert!(!generated.contains("\ntools ="));
    assert_eq!(
        config.agent_defaults.bundles,
        vec![
            psa_api::BundleConfig::FileTools { read_only: false },
            psa_api::BundleConfig::Bash,
            psa_api::BundleConfig::Subagents,
            psa_api::BundleConfig::Goal {
                interactive_only: true,
            },
        ]
    );
    assert_eq!(
        config.agent_defaults.hooks.agent_create,
        vec![
            psa_api::HookConfig::DefaultStartup,
            psa_api::HookConfig::ToolDefinitions,
            psa_api::HookConfig::Skills,
        ]
    );
    assert!(config.agent_defaults.hooks.session_update.is_empty());
    assert_eq!(
        config.agent_defaults.hooks.before_request,
        vec![psa_api::HookConfig::Compaction {
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            context_window: None,
        }]
    );
    assert!(config.agent_defaults.hooks.after_request.is_empty());
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
    assert!(!resolved.host);
    assert_eq!(resolved.default_agent_name, "default");
    // Existing files without a defaults section keep their original behavior.
    assert_eq!(resolved.agent_defaults, psa_api::AgentDefaults::default());
    assert_eq!(
        resolved.session_defaults,
        psa_server::config::default_model_config()
    );
}

#[tokio::test]
async fn config_reads_custom_default_agent_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let contents = "default_agent_name = \"helper\"\n[server]\nlisten = \"127.0.0.1:8080\"\n[database]\npath = \"test.sqlite3\"\n";
    tokio::fs::write(&path, contents).await.unwrap();
    let resolved = StartOptions {
        data_dir: Some(dir.path().into()),
        ..Default::default()
    }
    .resolve()
    .await
    .unwrap();
    assert_eq!(resolved.default_agent_name, "helper");
    assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), contents);
}

#[tokio::test]
async fn config_parses_local_probe_autostart_without_rewriting_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let options = || StartOptions {
        data_dir: Some(dir.path().into()),
        ..Default::default()
    };

    // New configurations enable the probe; missing fields in existing files remain disabled.
    assert!(options().resolve().await.unwrap().autostart_local_probe);
    for (setting, expected) in [
        ("", false),
        ("autostart_local_probe = true\n", true),
        ("autostart_local_probe = false\n", false),
    ] {
        let contents = format!(
            "[server]\n{setting}listen = '127.0.0.1:0'\n[database]\npath = 'psa.sqlite3'\n"
        );
        tokio::fs::write(&path, &contents).await.unwrap();
        assert_eq!(
            options().resolve().await.unwrap().autostart_local_probe,
            expected
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }

    for setting in ["'true'", "1", "[]"] {
        let contents = format!(
            "[server]\nautostart_local_probe = {setting}\n[database]\npath = 'psa.sqlite3'\n"
        );
        tokio::fs::write(&path, &contents).await.unwrap();
        assert!(matches!(
            options().resolve().await,
            Err(ConfigError::Parse { .. })
        ));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }
}

#[tokio::test]
async fn config_parses_host_marker_without_rewriting_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let base = "[server]\nlisten = '127.0.0.1:0'\n[database]\npath = 'psa.sqlite3'\n";
    let options = || StartOptions {
        data_dir: Some(dir.path().into()),
        ..Default::default()
    };

    for (marker, expected) in [
        ("", false),
        ("host = true\n", true),
        ("host = false\n", false),
    ] {
        let contents = format!("{marker}{base}");
        tokio::fs::write(&path, &contents).await.unwrap();
        assert_eq!(options().resolve().await.unwrap().host, expected);
        // Missing markers in existing configs stay false; no rewrite is expected.
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }

    for marker in ["host = 'true'\n", "host = 1\n", "host = []\n"] {
        let contents = format!("{marker}{base}");
        tokio::fs::write(&path, &contents).await.unwrap();
        assert!(matches!(
            options().resolve().await,
            Err(ConfigError::Parse { .. })
        ));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }
}

#[tokio::test]
async fn config_parses_agent_defaults_and_preserves_explicit_empty_lists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let base = "[server]\nlisten = \"127.0.0.1:0\"\n[database]\npath = \"psa.sqlite3\"\n";
    let options = || StartOptions {
        data_dir: Some(dir.path().into()),
        config: Some(path.clone()),
        ..Default::default()
    };
    tokio::fs::write(&path, format!("{base}\n[agent_defaults]\nbundles = [{{ FileTools = {{ read_only = false }} }}, \"Bash\"]\n[agent_defaults.hooks]\nagent_create = [\"DefaultStartup\", \"Skills\"]\nbefore_request = [\"Noop\"]\n"))
        .await.unwrap();
    let defaults = options().resolve().await.unwrap().agent_defaults;
    assert_eq!(
        defaults.bundles,
        vec![
            psa_api::BundleConfig::FileTools { read_only: false },
            psa_api::BundleConfig::Bash
        ]
    );
    assert_eq!(
        defaults.hooks.agent_create,
        vec![
            psa_api::HookConfig::DefaultStartup,
            psa_api::HookConfig::Skills
        ]
    );
    assert_eq!(
        defaults.hooks.before_request,
        vec![psa_api::HookConfig::Noop]
    );
    assert!(defaults.hooks.session_update.is_empty());
    assert!(defaults.hooks.after_request.is_empty());

    tokio::fs::write(
        &path,
        format!(
            "{base}\n[agent_defaults]\nbundles = []\n[agent_defaults.hooks]\nagent_create = []\n"
        ),
    )
    .await
    .unwrap();
    let defaults = options().resolve().await.unwrap().agent_defaults;
    assert!(defaults.bundles.is_empty());
    assert!(defaults.hooks.agent_create.is_empty());

    for tools in ["[]", "[\"Read\"]", "[\"InvalidTool\"]"] {
        tokio::fs::write(
            &path,
            format!("{base}\n[agent_defaults]\ntools = {tools}\n"),
        )
        .await
        .unwrap();
        assert!(matches!(
            options().resolve().await,
            Err(ConfigError::Parse { .. })
        ));
    }
}

#[tokio::test]
async fn config_parses_session_defaults_and_rejects_invalid_model_slugs() {
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let base = "[server]\nlisten = \"127.0.0.1:0\"\n[database]\npath = \"psa.sqlite3\"\n";
    let options = || StartOptions {
        data_dir: Some(dir.path().into()),
        config: Some(path.clone()),
        ..Default::default()
    };
    for (slug, provider, api_name, effort) in [
        (
            "openrouter/vendor/model",
            ProviderType::Openrouter,
            "vendor/model",
            Effort::Max,
        ),
        (
            "openai_codex/gpt-5",
            ProviderType::OpenAiCodex,
            "gpt-5",
            Effort::High,
        ),
    ] {
        let contents =
            format!("{base}\n[session_defaults]\nmodel = {slug:?}\neffort = \"{effort:?}\"\n");
        tokio::fs::write(&path, &contents).await.unwrap();
        let resolved = options().resolve().await.unwrap();
        assert_eq!(
            resolved.session_defaults,
            ModelConfig {
                model: Model::new(provider, api_name).unwrap(),
                effort,
            }
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }

    for fields in [
        "model = \"unknown/model\"\neffort = \"Medium\"",
        "model = \"gpt-5\"\neffort = \"Medium\"",
        "model = \"/gpt-5\"\neffort = \"Medium\"",
        "model = \"openai_codex/\"\neffort = \"Medium\"",
        "model = \"\"\neffort = \"Medium\"",
        "model = \" openrouter/vendor/model\"\neffort = \"Medium\"",
        "model = \"openrouter/vendor/model \"\neffort = \"Medium\"",
        "model = \"openrouter/ vendor/model\"\neffort = \"Medium\"",
        "model = \"openai_codex/gpt-5\"\neffort = \"Invalid\"",
        "model = \"openai_codex/gpt-5\"",
        "effort = \"Medium\"",
        "",
    ] {
        let contents = format!("{base}\n[session_defaults]\n{fields}\n");
        tokio::fs::write(&path, &contents).await.unwrap();
        assert!(
            matches!(options().resolve().await, Err(ConfigError::Parse { .. })),
            "accepted invalid session defaults: {fields}"
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
    }
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
            "test-client-key".into(),
            async {
                let _ = wait.await;
            },
        ));
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-client-key\r\nConnection: close\r\n\r\n")
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
