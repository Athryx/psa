mod support;

use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa_api::{Client, CreateSession, ForkPoint, ForkSession, Session, UpdateSession};
use psa_server::{
    config_registry::ConfigRegistry, db::Database, models::CatalogSource, probes::ProbeRegistry,
    providers::ProviderRegistry,
};
use serde_json::json;
use std::time::Duration;
use tokio::{net::TcpListener, sync::oneshot};

#[tokio::test]
async fn session_defaults_resolve_once_and_survive_updates_forks_and_server_restart() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let first_default = ModelConfig {
            model: Model::new(ProviderType::Openrouter, "vendor/first-default").unwrap(),
            effort: Effort::High,
        };
        let second_default = ModelConfig {
            model: Model::new(ProviderType::OpenAiCodex, "second-default").unwrap(),
            effort: Effort::Low,
        };
        let explicit = ModelConfig {
            model: Model::new(ProviderType::Openrouter, "vendor/explicit").unwrap(),
            effort: Effort::Minimal,
        };
        let mut earlier_sessions: Vec<Session> = Vec::new();

        for default in [first_default, second_default] {
            let database = Database::open(&database_path).await.unwrap();
            support::register_probe(&database).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let providers =
                ProviderRegistry::default().with_catalog_source(CatalogSource::new(&base).unwrap());
            let probes = ProbeRegistry::start("test-key".into());
            let (stop, stopped) = oneshot::channel();
            let server = tokio::spawn(psa_server::server::serve_with_configuration(
                listener,
                database.clone(),
                providers,
                probes,
                ConfigRegistry::new(directory.path().join("config")),
                default.clone(),
                "test-client-key".into(),
                async {
                    let _ = stopped.await;
                },
            ));
            let client = Client::new(&base, "test-client-key").unwrap();

            // A new server default must not replace a stored session's resolved model.
            for previous in &earlier_sessions {
                let found = client.get_session(previous.id).await.unwrap();
                assert_eq!(found.model_config, previous.model_config);
                let updated = client
                    .update_session(
                        previous.id,
                        &UpdateSession {
                            name: Some("renamed after restart".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(updated.model_config, previous.model_config);
                let fork = client
                    .fork_session(
                        previous.id,
                        &ForkSession {
                            point: ForkPoint::Head,
                            metadata: Default::default(),
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(fork.session.model_config, previous.model_config);
                assert_eq!(
                    client
                        .get_session(fork.session.id)
                        .await
                        .unwrap()
                        .model_config,
                    previous.model_config
                );
            }

            // Both an omitted field and explicit JSON null select this server's default.
            let http = reqwest::Client::new();
            for null_model in [false, true] {
                let mut request = json!({
                    "name": "defaulted",
                    "working_directory": "/work",
                    "probe_id": support::probe_metadata().id,
                });
                if null_model {
                    request["model_config"] = json!(null);
                }
                let response = http
                    .post(format!("{base}/sessions"))
                    .bearer_auth("test-client-key")
                    .json(&request)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                let body: serde_json::Value = response.json().await.unwrap();
                assert_eq!(
                    body["model_config"],
                    serde_json::to_value(&default).unwrap()
                );
                let session: Session = serde_json::from_value(body).unwrap();
                assert_eq!(
                    client.get_session(session.id).await.unwrap().model_config,
                    default
                );
                let mut cursor = database.cursor().await.unwrap();
                assert_eq!(
                    cursor.get_session(session.id).await.unwrap().model_config,
                    default
                );
                cursor.rollback().await.unwrap();
                earlier_sessions.push(session);
            }

            let override_session = client
                .create_session(&CreateSession {
                    name: "explicit".into(),
                    working_directory: "/work".into(),
                    probe_id: support::probe_metadata().id,
                    model_config: Some(explicit.clone()),
                })
                .await
                .unwrap();
            assert_eq!(override_session.model_config, explicit);
            assert_eq!(
                client
                    .get_session(override_session.id)
                    .await
                    .unwrap()
                    .model_config,
                explicit
            );
            earlier_sessions.push(override_session);

            stop.send(()).unwrap();
            server.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}
