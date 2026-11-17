//! Decision functions against a local fake decisions endpoint.
use super::*;
use crate::components::bundles::BundleConfig;
use crate::{
    agents::{
        NewAgent, agent_context::AgentContext, agent_manager::AgentManagerState,
        publication::Publication, session_manager::Runtime,
    },
    components::{
        bundles::decision_models::{DecisionAccess, DecisionHandle, DecisionPolicy},
        cancellation::Cancellation,
    },
    probes::ProbeRegistry,
    providers::ProviderRegistry,
    sessions::NewSession,
    test_support as support,
};
use llm_api::{
    api::decisions::Decisions,
    providers::{Model, ProviderType},
};
use psa_api::events::EventType;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn decider(name: &str) -> Model {
    Model::new(ProviderType::Openrouter, name).unwrap()
}

/// Serves the same response to every request and records each request body.
struct FakeEndpoint {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn fake_endpoint(status: &'static str, body: serde_json::Value) -> FakeEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/decisions", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let body = body.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let (header_end, length) = loop {
                let size = socket.read(&mut chunk).await.unwrap();
                if size == 0 {
                    break (0, 0);
                }
                bytes.extend_from_slice(&chunk[..size]);
                let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse().unwrap());
                break (end + 4, length);
            };
            while bytes.len() < header_end + length {
                let size = socket.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..size]);
            }
            if let Ok(value) = serde_json::from_slice(&bytes[header_end..header_end + length]) {
                recorded.lock().unwrap().push(value);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    FakeEndpoint { url, requests }
}

fn access(endpoint: Option<&FakeEndpoint>, include_global: bool, slugs: &[&str]) -> DecisionAccess {
    let providers = endpoint.map(|endpoint| {
        ProviderRegistry::default().with_decisions(vec![
            Decisions::with_url(ProviderType::Openrouter, &endpoint.url, "key").unwrap(),
        ])
    });
    let models: Vec<Model> = slugs.iter().map(|slug| slug.parse().unwrap()).collect();
    DecisionAccess {
        policy: Arc::new(DecisionPolicy::new(include_global, &models)),
        handle: DecisionHandle::new(providers, None),
    }
}

/// A session whose decision model is `vendor/session`, and an agent in it.
async fn with_context<T>(test: impl AsyncFnOnce(&mut AgentContext<'_>) -> T) -> T {
    let directory = tempfile::tempdir().unwrap();
    let database = crate::db::Database::open(directory.path().join("decisions.sqlite3"))
        .await
        .unwrap();
    support::register_probe(&database).await;
    let publication = Publication::default();
    let probes = ProbeRegistry::start("test-probe-key".into());
    let runtime = Runtime::start(
        database.clone(),
        publication.clone(),
        ProviderRegistry::default(),
        probes.clone(),
    )
    .await
    .unwrap();
    let mut cursor = database.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: Some(decider("vendor/session")),
            model_config: support::model_config(),
            name: "decisions".into(),
            working_directory: "/work".into(),
            probe_id: support::probe_metadata().id,
            project_id: None,
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
                name: "decisions".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut state = AgentManagerState::load_persisted(database.clone(), publication, agent.id)
        .await
        .unwrap();
    let mut context = AgentContext::with_optional_probe(&mut state, None, &runtime.handle);
    let output = test(&mut context).await;
    drop(context);
    runtime.shutdown().await.unwrap();
    database.close().await;
    output
}

fn arguments(model: Option<&str>) -> serde_json::Value {
    let mut arguments = json!({
        "state": "The package never arrived.",
        "questions": {
            "angry": {"type": "noul", "instructions": "Is the customer angry?"},
            "action": {
                "type": "choice",
                "instructions": "What should we do?",
                "criteria": {"refund": "Return the money", "resend": "Ship again"}
            },
            "tone": {"type": "score", "instructions": "How polite?", "criteria": ["rude", "polite"]}
        }
    });
    if let Some(model) = model {
        arguments["model"] = json!(model);
    }
    arguments
}

async fn run(
    function: &impl CodeModeFunction,
    context: &mut AgentContext<'_>,
    arguments: serde_json::Value,
) -> CodeModeFunctionResult {
    let (_sender, cancellation) = Cancellation::channel();
    function.execute(arguments, context, cancellation).await
}

fn response_body(cost: f64) -> serde_json::Value {
    json!({
        "id": "gen-dec-1",
        "model": "vendor/decider",
        "answers": {
            "angry": {"type": "noul", "noul": 0.25, "confidence": 0.9},
            "action": {"type": "choice", "choice": "refund",
                "probabilities": {"refund": 0.7, "resend": 0.3}},
            "tone": {"type": "refusal"}
        },
        "usage": {"input_tokens": 128, "output_tokens": 0, "cost": cost}
    })
}

async fn recorded_calls(context: &AgentContext<'_>) -> Vec<psa_api::DecisionCall> {
    context
        .load_all_events()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventType::DecisionCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn legal_models_are_configured_models_plus_the_session_model_when_included() {
    with_context(async |context| {
        let session = context.session();
        let configured = DecisionPolicy::new(false, &[decider("vendor/a")]);
        assert_eq!(configured.legal_models(session), vec![decider("vendor/a")]);

        let with_global = DecisionPolicy::new(true, &[decider("vendor/a"), decider("vendor/a")]);
        assert_eq!(
            with_global.legal_models(session),
            vec![decider("vendor/a"), decider("vendor/session")]
        );

        let only_global = DecisionPolicy::new(true, &[]);
        assert_eq!(
            only_global.legal_models(session),
            vec![decider("vendor/session")]
        );

        assert_eq!(configured.default_model(session), Some(decider("vendor/a")));
        assert_eq!(
            with_global.default_model(session),
            Some(decider("vendor/session")),
            "the session model is the default when included"
        );
        let several = DecisionPolicy::new(false, &[decider("vendor/a"), decider("vendor/b")]);
        assert_eq!(several.default_model(session), None);
    })
    .await;
}

#[tokio::test]
async fn listing_offers_only_legal_models_with_credentials() {
    let endpoint = fake_endpoint("200 OK", response_body(0.0)).await;
    with_context(async |context| {
        let function = list_decision_models::ListDecisionModels::new(access(
            Some(&endpoint),
            true,
            &["openrouter/vendor/a"],
        ));
        let listed = run(&function, context, json!({})).await.unwrap();
        assert_eq!(
            listed,
            json!([
                {"model": "openrouter/vendor/a"},
                {"model": "openrouter/vendor/session"},
            ])
        );

        let unavailable = list_decision_models::ListDecisionModels::new(access(
            None,
            true,
            &["openrouter/vendor/a"],
        ));
        assert_eq!(
            run(&unavailable, context, json!({})).await.unwrap(),
            json!([])
        );
    })
    .await;
}

#[tokio::test]
async fn decide_returns_answers_records_a_public_event_and_reports_cost() {
    let endpoint = fake_endpoint("200 OK", response_body(0.25)).await;
    with_context(async |context| {
        let access = access(Some(&endpoint), true, &["openrouter/vendor/decider"]);
        let function = decide::Decide::new(access.clone());

        let value = run(
            &function,
            context,
            arguments(Some("openrouter/vendor/decider")),
        )
        .await
        .unwrap();
        assert_eq!(value["model"], "openrouter/vendor/decider");
        assert_eq!(value["cost"], 0.25);
        assert_eq!(value["answers"]["angry"]["noul"], 0.25);
        assert_eq!(value["answers"]["action"]["choice"], "refund");
        assert_eq!(
            value["answers"]["tone"],
            serde_json::Value::Null,
            "a refusal is not an answer"
        );

        let request = endpoint.requests.lock().unwrap()[0].clone();
        assert_eq!(request["model"], "vendor/decider");
        assert_eq!(request["state"], "The package never arrived.");
        assert_eq!(request["questions"]["action"]["type"], "choice");

        let calls = recorded_calls(context).await;
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.model(), &decider("vendor/decider"));
        assert_eq!(call.request.state, "The package never arrived.");
        assert_eq!(
            call.request.questions.keys().collect::<Vec<_>>(),
            ["action", "angry", "tone"]
        );
        let response = call.response.as_ref().unwrap();
        assert!(!response.is_complete(&call.request));
        assert_eq!(response.usage.input_tokens, Some(128));
        assert_eq!(response.usage.cost, Some(0.25));
        assert_eq!(response.response_id.as_deref(), Some("gen-dec-1"));
        assert_eq!(
            response.answers.get("tone"),
            Some(&llm_api::api::decisions::DecisionAnswer::Refusal)
        );
        assert!(call.duration > std::time::Duration::ZERO);
    })
    .await;
}

#[tokio::test]
async fn decide_defaults_to_the_only_model_and_rejects_models_outside_the_policy() {
    let endpoint = fake_endpoint("200 OK", response_body(0.0)).await;
    with_context(async |context| {
        let function =
            decide::Decide::new(access(Some(&endpoint), false, &["openrouter/vendor/a"]));

        run(&function, context, arguments(None)).await.unwrap();
        assert_eq!(endpoint.requests.lock().unwrap()[0]["model"], "vendor/a");

        let error = run(
            &function,
            context,
            arguments(Some("openrouter/vendor/session")),
        )
        .await
        .unwrap_err();
        assert_eq!(error.exception_type, "ValueError");
        assert!(
            error.message.contains("openrouter/vendor/a"),
            "{}",
            error.message
        );

        let ambiguous = decide::Decide::new(access(
            Some(&endpoint),
            false,
            &["openrouter/vendor/a", "openrouter/vendor/b"],
        ));
        let error = run(&ambiguous, context, arguments(None)).await.unwrap_err();
        assert!(
            error.message.contains("model is required"),
            "{}",
            error.message
        );

        let with_global = decide::Decide::new(access(
            Some(&endpoint),
            true,
            &["openrouter/vendor/a", "openrouter/vendor/b"],
        ));
        run(&with_global, context, arguments(None)).await.unwrap();
        assert_eq!(
            endpoint.requests.lock().unwrap()[1]["model"],
            "vendor/session"
        );

        let mut invalid = arguments(Some("openrouter/vendor/a"));
        invalid["questions"] = json!({});
        let error = run(&function, context, invalid).await.unwrap_err();
        assert_eq!(error.exception_type, "ValueError");

        assert_eq!(
            endpoint.requests.lock().unwrap().len(),
            2,
            "rejected calls never reach the provider"
        );
        assert_eq!(recorded_calls(context).await.len(), 2);
    })
    .await;
}

#[tokio::test]
async fn failed_calls_raise_and_are_recorded() {
    let endpoint = fake_endpoint(
        "500 Internal Server Error",
        json!({"error": {"message": "boom"}}),
    )
    .await;
    with_context(async |context| {
        let function =
            decide::Decide::new(access(Some(&endpoint), false, &["openrouter/vendor/a"]));
        let error = run(&function, context, arguments(None)).await.unwrap_err();
        assert!(error.message.contains("boom"), "{}", error.message);

        let calls = recorded_calls(context).await;
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0].response, Err(message) if message.contains("boom")));

        let unavailable = decide::Decide::new(access(None, false, &["openrouter/vendor/a"]));
        let error = run(&unavailable, context, arguments(None))
            .await
            .unwrap_err();
        assert!(
            error.message.contains("No credentials"),
            "{}",
            error.message
        );
    })
    .await;
}

#[tokio::test]
async fn calls_beyond_the_concurrency_limit_queue_until_a_slot_frees() {
    // Accepts connections and never answers, so a sent call holds its slot.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/decisions", listener.local_addr().unwrap());
    let connections = Arc::new(Mutex::new(Vec::new()));
    let accepted = connections.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            accepted.lock().unwrap().push(socket);
        }
    });
    let providers = ProviderRegistry::default()
        .with_decisions(vec![
            Decisions::with_url(ProviderType::Openrouter, &url, "key").unwrap(),
        ])
        .with_max_concurrent_decisions(std::num::NonZeroUsize::MIN);
    let handle = DecisionHandle::new(Some(providers), None);
    let request = || llm_api::api::decisions::DecisionRequest {
        model: decider("vendor/a"),
        state: json!("state"),
        questions: [(
            "q".to_owned(),
            llm_api::api::decisions::DecisionQuestion::Noul {
                instructions: "decide".into(),
                criteria: None,
            },
        )]
        .into(),
    };

    let (first_sender, mut first_cancellation) = Cancellation::channel();
    let first = tokio::spawn({
        let (handle, request) = (handle.clone(), request());
        async move { handle.call(request, &mut first_cancellation).await }
    });
    while connections.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }

    let (_second_sender, mut second_cancellation) = Cancellation::channel();
    let second = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        handle.call(request(), &mut second_cancellation),
    )
    .await;
    assert!(second.is_err(), "the second call waits instead of failing");
    assert_eq!(
        connections.lock().unwrap().len(),
        1,
        "queued calls are not sent"
    );

    drop(first_sender);
    assert!(first.await.unwrap().unwrap_err().cancelled);
    let (_third_sender, mut third_cancellation) = Cancellation::channel();
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        handle.call(request(), &mut third_cancellation),
    )
    .await;
    assert_eq!(
        connections.lock().unwrap().len(),
        2,
        "a freed slot admits the next call"
    );
}

#[tokio::test]
async fn cancelled_calls_stop_without_recording() {
    let endpoint = fake_endpoint("200 OK", response_body(0.0)).await;
    with_context(async |context| {
        let function =
            decide::Decide::new(access(Some(&endpoint), false, &["openrouter/vendor/a"]));
        let (sender, cancellation) = Cancellation::channel();
        drop(sender);
        let error = function
            .execute(arguments(None), context, cancellation)
            .await
            .unwrap_err();
        assert_eq!(error, CodeModeError::runtime("Execution cancelled"));
        assert!(recorded_calls(context).await.is_empty());
    })
    .await;
}

#[test]
fn stubs_declare_dict_results_and_decide_is_async() {
    let access = access(None, false, &[]);
    let decide = CodeModeFunctionKind::Decide(decide::Decide::new(access.clone()));
    let list = CodeModeFunctionKind::ListDecisionModels(
        list_decision_models::ListDecisionModels::new(access),
    );
    assert!(decide.is_async());
    assert!(!list.is_async());

    let decide_stub = decide.python_stub().function;
    assert!(
        decide_stub.starts_with("async def decide("),
        "{decide_stub}"
    );
    assert!(decide_stub.contains("-> dict:"), "{decide_stub}");
    let list_stub = list.python_stub().function;
    assert!(
        list_stub.starts_with("def list_decision_models("),
        "{list_stub}"
    );
    assert!(list_stub.contains("-> list[dict]:"), "{list_stub}");
}

#[tokio::test]
async fn bundle_installs_help_only_functions_and_validates_its_configuration() {
    use crate::components::bundles::{
        BundleKind, InstalledComponents, session::SessionBundleResources,
    };
    let directory = tempfile::tempdir().unwrap();
    let database = crate::db::Database::open(directory.path().join("bundle.sqlite3"))
        .await
        .unwrap();
    let runtime = Runtime::start(
        database.clone(),
        Publication::default(),
        ProviderRegistry::default(),
        ProbeRegistry::start("test-probe-key".into()),
    )
    .await
    .unwrap();
    let (mut definition, agent, session) = registration_tests::snapshots();
    let mut resources = SessionBundleResources::start(session.id, runtime.handle.clone(), None);
    definition.bundles = vec![
        BundleConfig::DecisionModels {
            include_global: true,
            models: vec![decider("vendor/a")],
        },
        BundleConfig::CodeMode,
    ];

    let mut components = InstalledComponents::install_with_session(
        &definition,
        &agent,
        &session,
        Some(&resources.handle()),
    )
    .await
    .unwrap();
    let installed: Vec<_> = components
        .codemode_functions
        .iter()
        .map(|entry| (entry.function.definition().name, entry.listing))
        .collect();
    assert_eq!(
        installed,
        [
            ("list_decision_models".to_owned(), CodeModeListing::HelpOnly),
            ("decide".to_owned(), CodeModeListing::HelpOnly),
            ("prior_results".to_owned(), CodeModeListing::SystemPrompt),
            ("help".to_owned(), CodeModeListing::SystemPrompt),
        ]
    );
    components.cleanup().await.unwrap();

    let valid = BundleConfig::DecisionModels {
        include_global: false,
        models: Vec::new(),
    };
    assert!(
        BundleKind::from_config(&valid, None).is_err(),
        "session resources are required"
    );

    resources.cleanup().await.unwrap();
    runtime.shutdown().await.unwrap();
    database.close().await;
}
