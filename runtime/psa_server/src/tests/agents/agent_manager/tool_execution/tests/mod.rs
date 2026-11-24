use super::*;
use crate::{
    agents::{NewAgent, supervisor::Runtime},
    components::tools::{ToolId, ToolSystemPrompt, typed_tool_execute},
    http::AppState,
    test_support as support,
};
use psa_store::NewEvent;
use psa_store::NewSession;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
};

mod codemode;
#[path = "../../../../../../tests/support/fake_probe.rs"]
mod fake_probe;
mod monty_integration;
mod prior_results;
mod recovery;
mod reset_codemode;

#[derive(Debug)]
enum Notice {
    Enter(String, EventId, EventId, std::path::PathBuf),
    Saved(String, EventId),
    Rejected(String),
    Cancelled(String),
    Exit(String),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    label: String,
    #[serde(default)]
    saves: usize,
    #[serde(default)]
    stop: bool,
    #[serde(default)]
    panic: bool,
    #[serde(default)]
    invalid_save: bool,
}

pub struct ExecutionProbe {
    pub id: ToolId,
    mode: ExecutionMode,
    notices: mpsc::UnboundedSender<Notice>,
    release: Arc<Semaphore>,
    ignore_cancellation: bool,
    grace: Duration,
    dropped: Arc<AtomicUsize>,
}

struct InvocationDrop(Arc<AtomicUsize>);

impl Drop for InvocationDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ExecutionProbe {
    async fn execute_typed(
        &self,
        arguments: Args,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let _drop = InvocationDrop(self.dropped.clone());
        let label = arguments.label;
        let _ = self.notices.send(Notice::Enter(
            label.clone(),
            context.agent().next_event_id,
            context.projection().last_event_id,
            context.session().working_directory.clone(),
        ));
        if arguments.invalid_save {
            let batch = [
                EventBatchItem::Private(psa_store::PrivateEvent::CodeModeSnapshot(vec![7])),
                EventBatchItem::Public(NewEvent::new(EventType::Log(Log {
                    error: false,
                    message: "invalid-save-must-not-publish".into(),
                }))),
            ];
            assert!(matches!(
                context.save_event_batch(&batch).await,
                Err(AgentError::Database(DbError::InvalidInput(_)))
            ));
            let _ = self.notices.send(Notice::Rejected(label.clone()));
        }
        for index in 0..arguments.saves {
            let events = context
                .save_events(&[NewEvent::new(EventType::Log(Log {
                    error: false,
                    message: format!("{label}-{index}"),
                }))])
                .await
                .unwrap();
            assert_eq!(context.projection().last_event_id, events[0].id);
            assert_eq!(context.agent().next_event_id.0, events[0].id.0 + 1);
            let _ = self
                .notices
                .send(Notice::Saved(label.clone(), events[0].id));
        }
        if arguments.stop {
            context
                .save_events(&[status_event(AgentState::Idle)])
                .await
                .unwrap();
        }
        if arguments.panic {
            panic!("invocation panic");
        }
        tokio::select! {
            permit = self.release.acquire() => { permit.unwrap().forget(); }
            _ = cancellation.cancelled() => {
                let _ = self.notices.send(Notice::Cancelled(label.clone()));
                if self.ignore_cancellation { self.release.acquire().await.unwrap().forget(); }
                return ToolOutput::error(prompts::tool_interrupted());
            }
        }
        let _ = self.notices.send(Notice::Exit(label));
        ToolOutput::success(prompts::text("done"))
    }
}

impl Tool for ExecutionProbe {
    fn tool_definition(&self) -> api::ToolDefinition {
        let name = match self.id {
            ToolId::ListSessions => "parallel",
            ToolId::Read => "sequential",
            _ => "later",
        };
        api::ToolDefinition::for_type::<Args>(name, "Test controlled execution", Some(false))
    }
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt::default()
    }
    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }
    fn cancellation_timeout(&self) -> Duration {
        self.grace
    }
    typed_tool_execute!(Args, execute_typed);
}

struct Fixture {
    _directory: tempfile::TempDir,
    database: Database,
    publication: Publication,
    runtime: Runtime,
    probes: ProbeRegistry,
    probe: fake_probe::FakeProbe,
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("parallel.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&database).await;
        let publication = Publication::default();
        let probes = ProbeRegistry::start(fake_probe::PROBE_KEY.into());
        let runtime = Runtime::start(
            database.clone(),
            publication.clone(),
            ProviderRegistry::default(),
            probes.clone(),
        )
        .await
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = crate::http::router(AppState {
            database: database.clone(),
            publication: publication.clone(),
            runtime: runtime.handle.clone(),
            accepting: Arc::new(AtomicBool::new(true)),
            client_key: "test-client-key".into(),
            client_config: psa::api::ClientConfig {
                default_agent_name: "root".into(),
                default_sandbox_image: None,
            },
            probes: probes.clone(),
        });
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let probe = fake_probe::FakeProbe::connect(address, &probes).await;
        Self {
            _directory: directory,
            database,
            publication,
            runtime,
            probes,
            probe,
            stop,
            server,
        }
    }

    async fn state(
        &self,
        tools: Vec<ToolKind>,
        calls: &[(&str, &str, serde_json::Value)],
    ) -> AgentManagerState {
        let mut cursor = self.database.write().await.unwrap();
        let session = cursor
            .create_session(NewSession {
                decision_model: None,
                model_config: support::model_config(),
                probe_id: support::probe_metadata().id,
                project_id: None,
                name: "concurrent".into(),
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
                    definition_id: psa::api::AgentDefinitionId(1),
                    name: "test".into(),
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let mut state = AgentManagerState::load_persisted(
            self.database.clone(),
            self.publication.clone(),
            agent.id,
        )
        .await
        .unwrap();
        let mut events = vec![
            NewEvent::new(EventType::Model(support::model_config())),
            status_event(AgentState::Running),
            NewEvent::new(EventType::Message(Message::text(
                MessageType::Human,
                "run tools",
            ))),
        ];
        for tool in &tools {
            events.push(NewEvent::new(EventType::ToolDefinition(
                psa::api::events::ToolDefinition {
                    id: tool.id(),
                    definition: tool.tool_definition(),
                },
            )));
        }
        events.push(NewEvent::new(EventType::InferenceResultStart(
            InferenceResultStart {
                id: 100,
                model_config: support::model_config(),
                request_config: Default::default(),
            },
        )));
        for (id, name, arguments) in calls {
            events.push(NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: (*id).into(),
                tool_name: (*name).into(),
                arguments: arguments.to_string(),
                metadata: Default::default(),
            })));
        }
        events.push(NewEvent::new(EventType::InferenceResultEnd(
            InferenceResultEnd {
                id: 100,
                outcome: Outcome::Completed {
                    reason: api::StopReason::ToolCalls,
                },
                usage: Default::default(),
                request_time: Duration::ZERO,
                response_id: None,
                provider_metadata: Default::default(),
            },
        )));
        state.persist_events(&events).await.unwrap();
        state.tools = tools.into();
        state
    }

    async fn run(
        &self,
        mut state: AgentManagerState,
    ) -> (
        actors::UnboundedMailboxHandle<AgentManagerMessage>,
        JoinHandle<(AgentManagerState, Result<(), AgentError>)>,
    ) {
        let (controls, mut received) = actors::unbounded_channel();
        let probe = self.probes.handle(state.session.probe_id).await.unwrap();
        let sessions = self.runtime.handle.clone();
        let run = tokio::spawn(async move {
            let result = state.resolve_tools(&probe, &sessions, &mut received).await;
            (state, result)
        });
        (controls, run)
    }

    async fn close(self) {
        self.probe.disconnect(&self.probes).await;
        self.runtime.shutdown().await.unwrap();
        self.probes.shutdown().await;
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap();
        self.database.close().await;
    }
}

fn tool(
    id: ToolId,
    mode: ExecutionMode,
    notices: &mpsc::UnboundedSender<Notice>,
) -> (ToolKind, Arc<Semaphore>) {
    let release = Arc::new(Semaphore::new(0));
    (
        ToolKind::ExecutionProbe(ExecutionProbe {
            id,
            mode,
            notices: notices.clone(),
            release: release.clone(),
            ignore_cancellation: false,
            grace: Duration::from_secs(1),
            dropped: Arc::new(AtomicUsize::new(0)),
        }),
        release,
    )
}

async fn notice(notices: &mut mpsc::UnboundedReceiver<Notice>) -> Notice {
    tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn finish(run: JoinHandle<(AgentManagerState, Result<(), AgentError>)>) -> AgentManagerState {
    let (state, result) = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    assert!(state.outstanding_calls().is_empty());
    state
}

#[tokio::test]
async fn parallel_calls_overlap_with_stable_snapshots_and_call_id_results() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![parallel],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "parallel", serde_json::json!({"label":"b"})),
            ],
        )
        .await;
    let initial = state.agent.next_event_id;
    let (controls, run) = fixture.run(state).await;
    for _ in 0..2 {
        match notice(&mut notices).await {
            Notice::Enter(_, next, last, cwd) => {
                assert_eq!(next, initial);
                assert_eq!(last.0 + 1, next.0);
                assert_eq!(cwd, std::path::Path::new("/work"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    release.add_permits(2);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Running);
    let ids: Vec<_> = state
        .context
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"a") && ids.contains(&"b"));
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn sequential_calls_are_barriers_before_and_after() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, first) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let (sequential, middle) = tool(ToolId::Read, ExecutionMode::Sequential, &sender);
    let (later, last) = tool(ToolId::Edit, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![parallel, sequential, later],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "sequential", serde_json::json!({"label":"b"})),
                ("c", "later", serde_json::json!({"label":"c"})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "a"));
    assert!(notices.try_recv().is_err());
    first.add_permits(1);
    assert!(matches!(notice(&mut notices).await, Notice::Exit(label) if label == "a"));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "b"));
    assert!(notices.try_recv().is_err());
    middle.add_permits(1);
    assert!(matches!(notice(&mut notices).await, Notice::Exit(label) if label == "b"));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "c"));
    last.add_permits(1);
    finish(run).await;
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn one_control_consumer_cancels_every_active_call_and_skips_pending_calls() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let (sequential, _) = tool(ToolId::Read, ExecutionMode::Sequential, &sender);
    let state = fixture
        .state(
            vec![parallel, sequential],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                ("b", "parallel", serde_json::json!({"label":"b"})),
                ("c", "sequential", serde_json::json!({"label":"c"})),
            ],
        )
        .await;
    let mut session = state.session.clone();
    session.working_directory = "/updated".into();
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    controls
        .send(AgentManagerMessage::SessionUpdated { session })
        .unwrap();
    controls.send(AgentManagerMessage::Interrupt).unwrap();
    let mut cancelled = Vec::new();
    for _ in 0..2 {
        match notice(&mut notices).await {
            Notice::Cancelled(label) => cancelled.push(label),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(cancelled.contains(&"a".into()) && cancelled.contains(&"b".into()));
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Interrupted);
    assert_eq!(
        state.session.working_directory,
        std::path::Path::new("/updated")
    );
    assert!(notices.try_recv().is_err());
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn concurrent_saves_are_serialized_and_refresh_only_the_saving_invocation() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![parallel],
            &[
                ("a", "parallel", serde_json::json!({"label":"a", "saves":3})),
                ("b", "parallel", serde_json::json!({"label":"b", "saves":3})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let mut entered = 0;
    let mut saved = Vec::new();
    while saved.len() != 6 {
        match notice(&mut notices).await {
            Notice::Enter(..) => entered += 1,
            Notice::Saved(label, id) => {
                assert!(label == "a" || label == "b");
                saved.push(id.0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(entered, 2);
    saved.sort();
    assert!(saved.windows(2).all(|pair| pair[0] + 1 == pair[1]));
    release.add_permits(2);
    let state = finish(run).await;
    let reloaded = AgentManagerState::load_persisted(
        fixture.database.clone(),
        fixture.publication.clone(),
        state.agent.id,
    )
    .await
    .unwrap();
    assert_eq!(reloaded.agent.next_event_id, state.agent.next_event_id);
    assert_eq!(reloaded.context.last_event_id, state.context.last_event_id);
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn invalid_invocation_batch_replies_with_error_and_sibling_continues() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![parallel],
            &[
                (
                    "bad",
                    "parallel",
                    serde_json::json!({"label":"bad", "invalid_save":true}),
                ),
                (
                    "good",
                    "parallel",
                    serde_json::json!({"label":"good", "saves":1}),
                ),
            ],
        )
        .await;
    let mut stream = fixture.publication.lock().await.subscribe();
    let (controls, run) = fixture.run(state).await;
    let (mut rejected, mut saved) = (false, false);
    while !rejected || !saved {
        match notice(&mut notices).await {
            Notice::Enter(..) => {}
            Notice::Rejected(label) => {
                assert_eq!(label, "bad");
                rejected = true;
            }
            Notice::Saved(label, _) => {
                assert_eq!(label, "good");
                saved = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let mut committed = Vec::new();
    while let Ok(message) = stream.try_recv() {
        if let psa::api::StreamMessage::Committed { event, .. } = message {
            committed.push(event.event.kind);
        }
    }
    assert_eq!(committed.len(), 1);
    assert!(matches!(&committed[0], EventType::Log(log) if log.message == "good-0"));
    release.add_permits(2);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Running);
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn a_non_running_save_cancels_peers_and_preserves_the_intentional_status() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let state = fixture
        .state(
            vec![parallel],
            &[
                ("a", "parallel", serde_json::json!({"label":"a"})),
                (
                    "b",
                    "parallel",
                    serde_json::json!({"label":"b", "stop":true}),
                ),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Idle);
    let mut cancelled = 0;
    while let Ok(notice) = notices.try_recv() {
        if matches!(notice, Notice::Cancelled(_)) {
            cancelled += 1;
        }
    }
    assert_eq!(cancelled, 2);
    drop(controls);
    fixture.close().await;
}
