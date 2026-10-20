//! Real HTTP/client/SSE tests against an in-process fake inference endpoint.

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::StreamExt;
use llm_api::{
    api::{self, OpenAiCompletions, OpenAiCompletionsConfig, Provider},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::{self as wire, AgentRequest, AgentState, MessageDelivery, StreamMessage, events::*};
use psa_server::{
    agents::{
        NewAgent, ToolDefinitionExt, agent_manager::AgentManagerState, publication::Publication,
        session_manager::Runtime,
    },
    db::{Database, DbError},
    hooks::tools::ToolId,
    providers::ProviderRegistry,
    sessions::{ForkPoint, NewSession, SessionFork, SessionUpdate},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
};

fn model() -> ModelConfig {
    ModelConfig {
        provider: ProviderType::Openrouter,
        model: Model::new("test-model").unwrap(),
        effort: Effort::Medium,
    }
}

#[derive(Clone, Copy)]
enum Script {
    Text,
    Tools,
    Wait,
}

#[derive(Clone)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<Script>>>,
    requests: mpsc::UnboundedSender<Value>,
    stop: watch::Receiver<bool>,
}

async fn inference(State(mut fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let script = fake
        .scripts
        .lock()
        .await
        .pop_front()
        .unwrap_or(Script::Text);
    fake.requests.send(body).unwrap();
    let delta = match script {
        Script::Tools => json!({"role":"assistant", "tool_calls":[
            {"index":0,"id":"call-ok","type":"function","function":{"name":"noop","arguments":"{}"}},
            {"index":1,"id":"call-bad","type":"function","function":{"name":"noop","arguments":"{bad json"}}
        ]}),
        _ => json!({"role":"assistant", "content":"hello"}),
    };
    let first = format!(
        "data: {}\n\n",
        json!({"id":"fake-response","choices":[{"index":0,"delta":delta,"finish_reason":null}]})
    );
    let finish = if matches!(script, Script::Tools) {
        "tool_calls"
    } else {
        "stop"
    };
    let last = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})
    );
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(first);
        if matches!(script, Script::Wait) { let _ = fake.stop.changed().await; }
        else { yield Ok(last); }
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Database,
    client: wire::Client,
    requests: mpsc::UnboundedReceiver<Value>,
    stop: oneshot::Sender<()>,
    server: JoinHandle<Result<(), psa_server::server::ServerError>>,
    fake_stop: watch::Sender<bool>,
    fake_server: JoinHandle<()>,
}

impl Harness {
    async fn start(scripts: Vec<Script>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.sqlite3"))
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = listener.local_addr().unwrap();
        let (requests, received) = mpsc::unbounded_channel();
        let (fake_stop, stopped) = watch::channel(false);
        let app = Router::new()
            .route("/inference", post(inference))
            .with_state(Fake {
                scripts: Arc::new(Mutex::new(scripts.into())),
                requests,
                stop: stopped.clone(),
            });
        let fake_server = tokio::spawn(async move {
            let mut stopped = stopped;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
                .unwrap();
        });
        let provider = OpenAiCompletions::new(
            format!("http://{fake_address}/inference"),
            "fake-key",
            ProviderType::Openrouter,
            OpenAiCompletionsConfig::default(),
        )
        .unwrap();
        let providers = ProviderRegistry::new(vec![Provider::Completions(provider)]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(psa_server::server::serve_with_providers(
            listener,
            db.clone(),
            providers,
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            _dir: dir,
            db,
            client: wire::Client::new(&format!("http://{address}")).unwrap(),
            requests: received,
            stop,
            server,
            fake_stop,
            fake_server,
        }
    }

    async fn create_agent(&self) -> (wire::Session, wire::Agent) {
        let session = self
            .client
            .create_session(&wire::CreateSession {
                name: "session".into(),
                working_directory: "/work".into(),
            })
            .await
            .unwrap();
        let agent = self
            .client
            .create_agent(
                session.id,
                &wire::CreateAgent {
                    name: "agent".into(),
                    model: model(),
                    tools: vec![ToolId::Noop],
                },
            )
            .await
            .unwrap();
        (session, agent)
    }

    async fn wait_stopped(&self, session: wire::SessionId) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.client.get_session(session).await.unwrap().busy {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn close(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.fake_stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), self.fake_server)
            .await
            .unwrap()
            .unwrap();
    }
}

fn message(delivery: MessageDelivery) -> AgentRequest {
    AgentRequest::Message {
        delivery,
        content: psa_server::prompts::text("test input"),
    }
}

#[tokio::test]
async fn queued_requests_and_worker_retirement_do_not_lose_messages() {
    let mut harness = Harness::start(vec![]).await;
    let (session, agent) = harness.create_agent().await;
    // Repeatedly cross retirement boundaries, then race a burst of admissions.
    for _ in 0..4 {
        harness
            .client
            .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
            .await
            .unwrap();
        harness.requests.recv().await.unwrap();
        harness.wait_stopped(session.id).await;
    }
    let request = message(MessageDelivery::Queue);
    let replies = futures_util::future::join_all((0..12).map(|_| {
        harness
            .client
            .submit_request(session.id, agent.id, &request)
    }))
    .await;
    assert!(replies.iter().all(Result::is_ok));
    for _ in 0..12 {
        harness.requests.recv().await.unwrap();
    }
    harness.wait_stopped(session.id).await;
    let mut cursor = harness.db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Human)).count(), 16);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage.total_tokens,
        Some(16 * 7)
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn busy_queries_track_all_workers_and_request_ids_ignore_generations() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = Harness::start(vec![Script::Wait, Script::Wait]).await;
        let (session, first) = harness.create_agent().await;
        let second = harness
            .client
            .create_agent(
                session.id,
                &wire::CreateAgent {
                    name: "second".into(),
                    model: model(),
                    tools: vec![],
                },
            )
            .await
            .unwrap();
        assert!(!harness.client.get_session(session.id).await.unwrap().busy);

        for (index, agent) in [first.id, second.id].into_iter().enumerate() {
            let accepted = harness
                .client
                .submit_request(session.id, agent, &message(MessageDelivery::Queue))
                .await
                .unwrap();
            assert_eq!(
                accepted.request_id.rsplit('-').next().unwrap(),
                (index + 1).to_string()
            );
            harness.requests.recv().await.unwrap();
        }

        let updated = harness
            .client
            .update_session(
                session.id,
                &wire::UpdateSession {
                    name: Some("updated".into()),
                    working_directory: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "updated");
        assert!(updated.busy);
        assert!(harness.client.list_sessions(None).await.unwrap()[0].busy);

        harness
            .client
            .submit_request(session.id, first.id, &AgentRequest::Interrupt)
            .await
            .unwrap();
        while harness
            .client
            .get_agent(session.id, first.id)
            .await
            .unwrap()
            .state
            != AgentState::Interrupted
        {
            tokio::task::yield_now().await;
        }
        assert!(harness.client.get_session(session.id).await.unwrap().busy);

        harness
            .client
            .submit_request(session.id, second.id, &AgentRequest::Interrupt)
            .await
            .unwrap();
        harness.wait_stopped(session.id).await;
        assert!(!harness.client.list_sessions(None).await.unwrap()[0].busy);
        harness.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn interrupt_and_resume_restart_the_worker_without_replaying_the_request() {
    let mut harness = Harness::start(vec![Script::Wait, Script::Text]).await;
    let (session, agent) = harness.create_agent().await;
    assert!(matches!(
        harness
            .client
            .submit_request(session.id, agent.id, &message(MessageDelivery::Resume))
            .await,
        Err(wire::ClientError::Api { status: 409, .. })
    ));
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness
        .client
        .submit_request(session.id, agent.id, &AgentRequest::Interrupt)
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Interrupted
    );
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Resume))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
    harness.close().await;
}

#[tokio::test]
async fn http_tools_hooks_usage_streaming_and_reconnect() {
    let mut harness = Harness::start(vec![Script::Tools, Script::Text]).await;
    let (session, agent) = harness.create_agent().await;
    let query = wire::EventsQuery {
        limit: 1,
        include_pending: true,
        ..Default::default()
    };
    let mut stream = harness
        .client
        .stream_events(session.id, None, &query, None)
        .await
        .unwrap();
    let mut durable = Vec::new();
    let mut resume_id = String::new();
    // A paginated initial replay must remain connected afterward.
    loop {
        let event = stream.next().await.unwrap().unwrap();
        match event.message {
            StreamMessage::Committed { event, .. } => durable.push(event),
            StreamMessage::Snapshot { .. } => {
                resume_id.clone_from(&event.id);
                break;
            }
            _ => {}
        }
    }
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    let first = harness.requests.recv().await.unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 1);
    let mut pending_seen = false;
    let mut promoted = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = stream.next().await.unwrap().unwrap();
            match event.message {
                StreamMessage::Upsert { .. } => pending_seen = true,
                StreamMessage::Committed { event, pending_id } => {
                    promoted |= pending_id.is_some();
                    durable.push(event);
                }
                StreamMessage::SessionBusy { busy: false, .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(pending_seen && promoted);
    let second = harness.requests.recv().await.unwrap();
    assert!(
        second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool")
    );
    let agent_view = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(agent_view.state, AgentState::Idle);
    assert_eq!(agent_view.usage.total_tokens, Some(14));
    let page = harness
        .client
        .events(
            session.id,
            None,
            &wire::EventsQuery {
                limit: 10_000,
                include_pending: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.pending.is_empty());
    assert_eq!(durable.len(), page.events.len());
    assert!(
        durable
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
    let results: Vec<_> = page
        .events
        .iter()
        .filter_map(|event| match &event.event.kind {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(!results[0].error);
    assert!(results[1].error);
    let mut reconnected = harness
        .client
        .stream_events(
            session.id,
            None,
            &wire::EventsQuery::default(),
            Some(&resume_id),
        )
        .await
        .unwrap();
    let mut replayed = 0;
    loop {
        match reconnected.next().await.unwrap().unwrap().message {
            StreamMessage::Committed { .. } => replayed += 1,
            StreamMessage::Snapshot { .. } => break,
            _ => {}
        }
    }
    assert_eq!(replayed, page.events.len() - 2); // initial Model + ToolDefinition
    let other = harness
        .client
        .create_session(&wire::CreateSession {
            name: "other".into(),
            working_directory: "/other".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        harness.client.get_agent(other.id, agent.id).await,
        Err(wire::ClientError::Api { status: 404, .. })
    ));
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(!fork.session.busy);
    assert_eq!(fork.agents[0].1.usage, agent_view.usage);
    // Leave SSE connections open: shutdown must close them, not hang draining.
    harness.close().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn shutdown_interrupts_inference_persists_partial_output_and_discards_queue() {
    let mut harness = Harness::start(vec![Script::Wait]).await;
    let (session, agent) = harness.create_agent().await;
    let path = harness._dir.path().join("test.sqlite3");
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    assert!(harness.client.get_session(session.id).await.unwrap().busy);
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(!fork.session.busy);
    assert_eq!(fork.agents[0].1.state, AgentState::Interrupted);
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    // Wait until the initial partial item has actually reached the worker.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let page = harness
                .client
                .events(
                    session.id,
                    Some(agent.id),
                    &wire::EventsQuery {
                        include_pending: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            if !page.pending.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    harness.stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), harness.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(harness.requests.try_recv().is_err());
    let db = Database::open(path).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Human)).count(), 1);
    assert!(events.iter().any(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Agent)));
    assert!(events.iter().any(|event| matches!(&event.kind, EventType::InferenceResultEnd(end) if matches!(end.outcome, api::Outcome::Cancelled))));
    cursor.rollback().await.unwrap();
    db.close().await;
    harness.fake_stop.send_replace(true);
    harness.fake_server.await.unwrap();
}

fn inference_events(id: u64) -> Vec<NewEvent> {
    vec![
        NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
            id,
            model_config: model(),
            request_config: Default::default(),
        })),
        NewEvent::new(EventType::ToolCall(ToolCall {
            call_id: format!("call-{id}"),
            tool_name: "noop".into(),
            arguments: "{}".into(),
            metadata: Default::default(),
        })),
        NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
            id,
            outcome: api::Outcome::Completed {
                reason: api::StopReason::ToolCalls,
            },
            usage: api::Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                cached_input_tokens: Some(0),
                reasoning_output_tokens: Some(0),
                ..Default::default()
            },
            request_time: Default::default(),
            response_id: None,
            provider_metadata: Default::default(),
        })),
    ]
}

#[tokio::test]
async fn cursor_windows_fork_usage_and_idempotent_orphan_repair() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("db.sqlite3")).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            name: "root".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                name: "agent".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        cursor
            .load_context_window(agent.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        cursor
            .load_last_inference_result(agent.id)
            .await
            .unwrap()
            .is_none()
    );
    cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::NewContext),
                NewEvent::new(EventType::Model(model())),
                NewEvent::new(EventType::ToolDefinition(ToolDefinition::for_tool(
                    ToolId::Noop,
                ))),
            ],
        )
        .await
        .unwrap();
    let added = cursor
        .add_events(agent.id, &inference_events(1))
        .await
        .unwrap();
    assert_eq!(added.new_usage.as_ref().unwrap().total_tokens, Some(15));
    let end_id = added.events.last().unwrap().id;
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: end_id,
                },
                metadata: SessionUpdate::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0].1;
    assert_eq!(child.usage.total_tokens, Some(15));
    let incomplete = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: EventId(end_id.0 - 1),
                },
                metadata: SessionUpdate::default(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        cursor
            .load_last_inference_result(incomplete.agents[0].1.id)
            .await,
        Err(DbError::IncompleteInference(1))
    ));
    let logs: Vec<_> = (0..10100)
        .map(|_| {
            NewEvent::new(EventType::Log(Log {
                error: false,
                message: "log".into(),
            }))
        })
        .collect();
    cursor.add_events(agent.id, &logs).await.unwrap();
    assert_eq!(
        cursor.load_context_window(agent.id).await.unwrap().len(),
        10106
    );
    assert_eq!(
        cursor
            .load_events_since_last_inference(agent.id)
            .await
            .unwrap()
            .unwrap()
            .len(),
        10103
    );
    let response = cursor
        .load_last_inference_result(child.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.items.len(), 1);
    assert_eq!(response.end.id, end_id);
    cursor.commit().await.unwrap();

    let publication = Publication::default();
    let runtime = Runtime::start(db.clone(), publication, ProviderRegistry::default())
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let child_events = cursor.load_all_events(child.id).await.unwrap();
    let repaired_len = child_events.len();
    assert_eq!(
        child_events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::ToolResult(result) if result.error))
            .count(),
        1
    );
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().state,
        AgentState::Interrupted
    );
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().usage.total_tokens,
        Some(15)
    );
    cursor.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
    let runtime = Runtime::start(
        db.clone(),
        Publication::default(),
        ProviderRegistry::default(),
    )
    .await
    .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.load_all_events(child.id).await.unwrap().len(),
        repaired_len
    );
    cursor.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn persistence_failure_does_not_mutate_worker_memory() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    let db = Database::open(&path).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            name: "root".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                name: "agent".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut state = AgentManagerState::load(db.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_append BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'test failure'); END").execute(&mut connection).await.unwrap();
    assert!(
        state
            .persist_events(&[NewEvent::new(EventType::StatusChange(StatusChange {
                new_status: AgentState::Running,
                reason: None
            }))])
            .await
            .is_err()
    );
    assert_eq!(state.agent().state, AgentState::Idle);
    assert_eq!(state.context().last_event_id, EventId(0));
    let mut cursor = db.cursor().await.unwrap();
    assert!(cursor.load_all_events(agent.id).await.unwrap().is_empty());
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Idle
    );
    cursor.rollback().await.unwrap();
    connection.close().await.unwrap();
    db.close().await;
}
