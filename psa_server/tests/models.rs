//! Real HTTP/client tests with isolated catalog and inference sources.
mod support;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use llm_api::{
    api::{OpenAiCompletions, OpenAiCompletionsConfig, Provider, ReasoningOverride},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::{
    self as api,
    events::{EventRange, EventType},
};
use psa_server::{db::Database, models::CatalogSource, providers::ProviderRegistry};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};

#[derive(Clone)]
struct Fake {
    fail: Arc<AtomicBool>,
    requests: mpsc::UnboundedSender<Value>,
}

async fn models(State(fake): State<Fake>) -> Response {
    if fake.fail.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(json!([{"id":"vendor/model","name":"Catalog model","type":"chat","reasoning":true,
        "thinkingLevelMap":{"off":null,"minimal":"low","max":"max"},"contextWindow":128000,"maxTokens":8192,
        "cost":{"input":2.5,"output":10,"cacheRead":0.25,"cacheWrite":0}}])).into_response()
}

async fn inference(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    fake.requests.send(body).unwrap();
    let data = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(data))
        .unwrap()
}

async fn wait_idle(client: &api::Client, session: api::SessionId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.get_session(session).await.unwrap().busy {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn catalog_endpoints_pins_and_reasoning_execution_work_through_the_client() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("api.sqlite3"))
        .await
        .unwrap();
    support::register_probe(&database).await;
    let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_base = format!("http://{}", fake_listener.local_addr().unwrap());
    let (requests, mut received) = mpsc::unbounded_channel();
    let fail = Arc::new(AtomicBool::new(false));
    let fake_app = Router::new()
        .route("/api/models/providers/{provider}", get(models))
        .route("/inference", post(inference))
        .with_state(Fake {
            fail: fail.clone(),
            requests,
        });
    let fake_server = tokio::spawn(async move {
        axum::serve(fake_listener, fake_app).await.unwrap();
    });
    let provider = Provider::Completions(
        OpenAiCompletions::new(
            format!("{fake_base}/inference"),
            "test-key",
            ProviderType::Openrouter,
            OpenAiCompletionsConfig {
                openrouter_reasoning_fields: true,
            },
        )
        .unwrap(),
    );
    let providers = ProviderRegistry::new(vec![provider])
        .with_catalog_source(CatalogSource::new(&fake_base).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = api::Client::new(&format!("http://{address}")).unwrap();
    let (stop, stopped) = oneshot::channel();
    let probes = psa_server::probes::ProbeRegistry::start("test-key".into());
    let server = tokio::spawn(psa_server::server::serve_with_probe_registry(
        listener,
        database.clone(),
        providers,
        probes.clone(),
        async {
            let _ = stopped.await;
        },
    ));
    // The worker requires a connected probe even when no probe tools are used.
    let probe = fake_probe::FakeProbe::connect(address, &probes).await;
    let refreshed = client
        .resync_models(&api::ResyncModels::default())
        .await
        .unwrap();
    assert_eq!(refreshed.len(), 2);
    assert!(refreshed.iter().all(|result| result.success));
    let providers = client.list_providers().await.unwrap();
    assert_eq!(providers.len(), 2);
    assert!(
        providers
            .iter()
            .find(|provider| provider.id == ProviderType::Openrouter)
            .unwrap()
            .authed
    );
    assert!(
        !providers
            .iter()
            .find(|provider| provider.id == ProviderType::OpenAiCodex)
            .unwrap()
            .authed
    );
    assert_eq!(
        client
            .list_models(&api::ListModels {
                authed: Some(false),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    let config = ModelConfig {
        provider: ProviderType::Openrouter,
        model: Model::new("vendor/model").unwrap(),
        effort: Effort::Minimal,
    };
    let info = client.model_info(&config).await.unwrap();
    assert_eq!(info.name, "Catalog model");
    assert_eq!(info.cost.rates.input, Some(2.5));
    assert_eq!(info.context_window, Some(128000));
    let pin = api::PinModel {
        provider: config.provider,
        model: config.model.clone(),
        pinned: true,
    };
    assert!(client.set_model_pinned(&pin).await.unwrap().unwrap().pinned);
    assert_eq!(
        client
            .list_models(&api::ListModels {
                pinned: Some(true),
                authed: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        client
            .list_models(&api::ListModels {
                pinned: Some(false),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    let mut missing = config.clone();
    missing.model = Model::new("unknown/model").unwrap();
    assert!(matches!(
        client.model_info(&missing).await,
        Err(api::ClientError::Api { status: 404, .. })
    ));
    let missing_pin = api::PinModel {
        provider: config.provider,
        model: missing.model.clone(),
        pinned: true,
    };
    assert!(matches!(
        client.set_model_pinned(&missing_pin).await,
        Err(api::ClientError::Api { status: 404, .. })
    ));
    assert!(
        client
            .set_model_pinned(&api::PinModel {
                pinned: false,
                ..missing_pin
            })
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reqwest::get(format!("http://{address}/models?authed=invalid"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    let definition = client
        .create_agent_definition(&api::CreateAgentDefinition {
            name: "catalog-aware".into(),
            hooks: api::HookIds {
                agent_create: vec![api::HookId::DefaultStartup],
                after_request: vec![api::HookId::StopWithoutTools],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    for effort in [Effort::Minimal, Effort::Off] {
        let session = client
            .create_session(&api::CreateSession {
                name: format!("{effort:?}"),
                working_directory: "/work".into(),
                probe_id: support::probe_metadata().id,
                model_config: ModelConfig {
                    effort,
                    ..config.clone()
                },
            })
            .await
            .unwrap();
        let agent = client
            .create_agent(
                session.id,
                &api::CreateAgent {
                    subagent_parent_id: None,
                    name: "main".into(),
                    definition_id: definition.id,
                    interactive: false,
                },
            )
            .await
            .unwrap();
        client
            .send_subagent_message(
                session.id,
                agent.id,
                None,
                psa_server::prompts::text("hello"),
            )
            .await
            .unwrap();
        wait_idle(&client, session.id).await;
        let page = client
            .events(session.id, Some(agent.id), &Default::default())
            .await
            .unwrap();
        let start = page
            .events
            .iter()
            .find_map(|envelope| match &envelope.event.kind {
                EventType::InferenceResultStart(start) => Some(start),
                _ => None,
            })
            .unwrap();
        assert_eq!(start.model_config.effort, effort);
        if effort == Effort::Minimal {
            let body = received.recv().await.unwrap();
            assert_eq!(body["reasoning"]["effort"], "low");
            assert_eq!(
                start.request_config.reasoning,
                Some(ReasoningOverride::Effort(Effort::Low))
            );
        } else {
            assert!(
                received.try_recv().is_err(),
                "unsupported effort must not call the provider"
            );
            assert!(page.events.iter().any(|envelope|matches!(&envelope.event.kind,
                EventType::InferenceResultEnd(end) if matches!(&end.outcome,llm_api::api::Outcome::Failed(error) if error.to_string().contains("Supported:")))));
        }
        // Trace reads retain the resolved request even through the core database API.
        let mut cursor = database.cursor().await.unwrap();
        assert!(
            !cursor
                .load_events(agent.id, EventRange::default())
                .await
                .unwrap()
                .is_empty()
        );
        cursor.rollback().await.unwrap();
    }
    fail.store(true, Ordering::SeqCst);
    let failed = client
        .resync_models(&api::ResyncModels {
            provider: Some(config.provider),
        })
        .await
        .unwrap();
    assert_eq!(failed.len(), 1);
    assert!(!failed[0].success);
    assert!(client.model_info(&config).await.unwrap().pinned);
    probe.disconnect(&probes).await;
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fake_server.abort();
}

#[path = "support/fake_probe.rs"]
mod fake_probe;
