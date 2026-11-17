//! Exercise persistence -> projection -> both real provider serializers over loopback.

mod support;
use axum::{Json, Router, extract::State, http::header, routing::post};
use llm_api::{
    api::{
        ApiProvider, OpenAiCompletions, OpenAiCompletionsConfig, OpenAiResponses, RequestConfig,
        Usage,
    },
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::events::*;
use psa_server::db::{EventRange, NewEvent};
use psa_server::{
    agents::NewAgent, agents::agent_projection::AgentProjection, db::Database, sessions::NewSession,
};
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};

async fn capture(
    State(sender): State<mpsc::Sender<Value>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    sender.send(body).await.unwrap();
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        "data: [DONE]\n\n",
    )
}

#[tokio::test]
async fn compacted_database_context_serializes_once_in_both_protocols() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&db).await;
        let mut cursor = db.cursor().await.unwrap();
        let session = cursor
            .create_session(NewSession {
                decision_model: None,
                model_config: support::model_config(),
                probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
                project_id: None,
                name: "test".into(),
                working_directory: "/work".into(),
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    definition_id: psa_api::AgentDefinitionId(1),
                    name: "main".into(),
                },
            )
            .await
            .unwrap();
        cursor
            .add_events(
                agent.id,
                &[
                    NewEvent::new(EventType::NewContext),
                    NewEvent::new(EventType::Model(ModelConfig {
                        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
                        effort: Effort::High,
                    })),
                    NewEvent::new(EventType::Compaction(Compaction {
                        summary: CompactionSummary::Text("previous work".into()),
                        usage: Usage::default(),
                    })),
                    NewEvent::new(EventType::Message(Message::text(
                        MessageType::Agent,
                        "retained reply",
                    ))),
                    NewEvent::new(EventType::Message(Message::text(
                        MessageType::Human,
                        "continue",
                    ))),
                ],
            )
            .await
            .unwrap();
        let state = AgentProjection::replay(
            &cursor
                .load_events(agent.id, EventRange::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let request = state
            .prepare_request(RequestConfig {
                reasoning: None,
                max_output_tokens: Some(42),
                ..Default::default()
            })
            .unwrap();
        cursor.commit().await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, mut received) = mpsc::channel(2);
        let (stop, stopped) = oneshot::channel();
        let app = Router::new()
            .route("/responses", post(capture))
            .route("/completions", post(capture))
            .with_state(sender);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });

        let responses = OpenAiResponses::new(
            format!("http://{address}/responses"),
            "test",
            ProviderType::Openrouter,
        )
        .unwrap();
        drop(
            responses
                .generate(&request.history, &request.config)
                .await
                .unwrap(),
        );
        let body = received.recv().await.unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"][0]["text"], "retained reply");
        assert_eq!(body["max_output_tokens"], 42);

        let completions = OpenAiCompletions::new(
            format!("http://{address}/completions"),
            "test",
            ProviderType::Openrouter,
            OpenAiCompletionsConfig::default(),
        )
        .unwrap();
        drop(
            completions
                .generate(&request.history, &request.config)
                .await
                .unwrap(),
        );
        let body = received.recv().await.unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["max_completion_tokens"], 42);
        stop.send(()).unwrap();
        server.await.unwrap();
        db.close().await;
    })
    .await
    .unwrap();
}
