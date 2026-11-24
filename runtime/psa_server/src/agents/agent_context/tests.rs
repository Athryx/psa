use super::*;
use psa_store::{EventRange, NewEvent};

#[path = "client_query_tests.rs"]
mod client_query_tests;
use crate::{
    agents::{
        AgentState, NewAgent,
        agent_manager::AgentClient,
        publication::{Publication, status_event},
        supervisor::Runtime,
    },
    components::{
        cancellation::Cancellation,
        hooks::{Hook, HookError, StopWithoutTools, run_hook},
        tools::{Edit, Read, Tool, ToolOutput, Write},
    },
    http::AppState,
    probes::ProbeRegistry,
    test_support as support,
};
use llm_api::{
    api::{self, Outcome, StopReason},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa::api::StreamMessage;
use psa_models::ProviderRegistry;
use psa_store::{ForkPoint, NewSession, SessionFork};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

#[path = "../../../tests/support/fake_probe.rs"]
mod fake_probe;

struct Fixture {
    _directory: tempfile::TempDir,
    database: psa_store::Database,
    publication: Publication,
    client: psa::api::Client,
    base: String,
    runtime: Runtime,
    probes: ProbeRegistry,
    probe: fake_probe::FakeProbe,
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        Self::new_with_probe_files(None).await
    }

    async fn new_with_files(files: fake_probe::FakeFiles) -> Self {
        Self::new_with_probe_files(Some(files)).await
    }

    async fn new_with_probe_files(files: Option<fake_probe::FakeFiles>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = psa_store::Database::open(directory.path().join("context.sqlite3"))
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
        let probe = match files {
            Some(files) => fake_probe::FakeProbe::connect_with_files(address, &probes, files).await,
            None => fake_probe::FakeProbe::connect(address, &probes).await,
        };

        Self {
            _directory: directory,
            database,
            publication,
            client: psa::api::Client::new(&format!("http://{address}"), "test-client-key").unwrap(),
            base: format!("http://{address}"),
            runtime,
            probes,
            probe,
            stop,
            server,
        }
    }

    async fn create_agent(&self, events: &[NewEvent]) -> Agent {
        self.create_agent_in("/work", events).await
    }

    async fn create_agent_in(&self, cwd: &str, events: &[NewEvent]) -> Agent {
        let mut cursor = self.database.write().await.unwrap();
        let session = cursor
            .create_session(NewSession {
                decision_model: None,
                model_config: crate::test_support::model_config(),
                name: "context test".into(),
                working_directory: cwd.into(),
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
                    definition_id: psa::api::AgentDefinitionId(1),
                    name: "agent".into(),
                },
            )
            .await
            .unwrap();
        cursor.add_events(agent.id, events).await.unwrap();
        let agent = cursor.get_agent(agent.id).await.unwrap();
        cursor.commit().await.unwrap();
        agent
    }

    async fn load_state(&self, agent: &Agent) -> AgentManagerState {
        AgentManagerState::load(self.database.clone(), self.publication.clone(), agent.id)
            .await
            .unwrap()
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

fn model() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::Medium,
    }
}

fn log(message: &str) -> NewEvent {
    NewEvent::new(EventType::Log(Log {
        error: false,
        message: message.into(),
    }))
}

fn inference_events(tool_call: bool) -> Vec<NewEvent> {
    let mut events = vec![
        NewEvent::new(EventType::Model(model())),
        NewEvent::new(EventType::Message(Message::text(
            MessageType::Human,
            "question",
        ))),
        NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
            id: 10,
            model_config: model(),
            request_config: Default::default(),
        })),
    ];
    events.push(NewEvent::new(if tool_call {
        EventType::ToolCall(ToolCall {
            call_id: "call-1".into(),
            tool_name: "noop".into(),
            arguments: "{}".into(),
            metadata: Default::default(),
        })
    } else {
        EventType::Message(Message::text(MessageType::Agent, "answer"))
    }));
    events.push(NewEvent::new(EventType::InferenceResultEnd(
        InferenceResultEnd {
            id: 10,
            outcome: Outcome::Completed {
                reason: if tool_call {
                    StopReason::ToolCalls
                } else {
                    StopReason::EndTurn
                },
            },
            usage: Default::default(),
            request_time: Duration::ZERO,
            response_id: Some("response-10".into()),
            provider_metadata: Default::default(),
        },
    )));
    events
}

fn assert_events(actual: &[Event], expected: &[Event]) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[tokio::test]
async fn trace_queries_match_database_and_bind_inherited_fork_history() {
    let fixture = Fixture::new().await;
    let mut transcript = vec![log("before reset"), NewEvent::new(EventType::NewContext)];
    transcript.extend(inference_events(false));
    transcript.push(log("parent tail"));
    let parent = fixture.create_agent(&transcript).await;
    let mut cursor = fixture.database.write().await.unwrap();
    let other = cursor
        .create_agent(
            parent.session_id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa::api::AgentDefinitionId(1),
                name: "other".into(),
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(other.id, &[log("other agent private")])
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            parent.session_id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = fork
        .agents
        .iter()
        .find(|agent| agent.forked_from == Some(parent.id))
        .unwrap()
        .clone();
    cursor
        .add_events(parent.id, &[log("parent after fork private")])
        .await
        .unwrap();
    cursor
        .add_events(child.id, &[log("child only")])
        .await
        .unwrap();
    let range = EventRange {
        after: EventId(2),
        through: Some(EventId(7)),
        limit: 3,
    };
    let expected_page = cursor.load_events(child.id, range.clone()).await.unwrap();
    let expected_all = cursor.load_all_events(child.id).await.unwrap();
    let expected_window = cursor.load_context_window(child.id).await.unwrap();
    let expected_since = cursor
        .load_events_since_last_inference(child.id)
        .await
        .unwrap()
        .unwrap();
    let expected_result = cursor
        .load_last_inference_result(child.id)
        .await
        .unwrap()
        .unwrap();
    let expected_envelopes = cursor
        .load_agent_envelopes(child.session_id, child.id, range.clone())
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let mut state = fixture.load_state(&child).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    assert_eq!(context.agent().id, child.id);
    assert_eq!(context.session().id, child.session_id);
    assert_eq!(context.session().probe_id, support::probe_metadata().id);
    assert!(std::ptr::eq(context.probe().unwrap(), &probe));
    assert_eq!(
        context
            .runtime()
            .get_agent(child.session_id, child.id)
            .await
            .unwrap()
            .id,
        child.id
    );
    assert_events(
        &context.load_events(range.clone()).await.unwrap(),
        &expected_page,
    );
    assert_events(&context.load_all_events().await.unwrap(), &expected_all);
    assert_events(
        &context.load_context_window().await.unwrap(),
        &expected_window,
    );
    assert_events(
        &context
            .load_events_since_last_inference()
            .await
            .unwrap()
            .unwrap(),
        &expected_since,
    );
    let result = context.load_last_inference_result().await.unwrap().unwrap();
    assert_events(
        &[result.start, result.end],
        &[expected_result.start, expected_result.end],
    );
    assert_events(&result.items, &expected_result.items);
    assert_eq!(
        serde_json::to_value(context.load_agent_envelopes(range).await.unwrap()).unwrap(),
        serde_json::to_value(expected_envelopes).unwrap()
    );
    assert!(
        expected_all
            .iter()
            .any(|event| event.source_agent_id == parent.id)
    );
    assert!(
        expected_all
            .iter()
            .any(|event| event.source_agent_id == child.id)
    );
    assert!(
        expected_all
            .iter()
            .all(|event| event.source_agent_id != other.id)
    );
    assert!(
        !serde_json::to_string(&expected_all)
            .unwrap()
            .contains("private")
    );
    assert!(matches!(expected_window[0].kind, EventType::NewContext));
    fixture.close().await;
}

#[tokio::test]
async fn saved_events_synchronize_projection_agent_status_and_publication() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut published = fixture.publication.lock().await.subscribe();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let saved = context
        .save_events(&[
            log("saved by context"),
            status_event(AgentState::Running),
            status_event(AgentState::Running),
            status_event(AgentState::Interrupted),
            status_event(AgentState::Interrupted),
        ])
        .await
        .unwrap();
    assert_eq!(saved.len(), 3);
    assert_eq!(context.agent().state, AgentState::Interrupted);
    assert_eq!(context.projection().status, AgentState::Interrupted);
    assert_eq!(context.projection().last_event_id, EventId(3));
    assert_eq!(context.agent().next_event_id, EventId(4));
    assert_events(&context.load_all_events().await.unwrap(), &saved);
    assert!(
        context
            .save_events(&[status_event(AgentState::Interrupted)])
            .await
            .unwrap()
            .is_empty()
    );
    for event in &saved {
        let StreamMessage::Committed {
            event: envelope,
            pending_id,
        } = published.try_recv().unwrap()
        else {
            panic!("expected committed context event");
        };
        assert_eq!(envelope.agent_id, agent.id);
        assert!(pending_id.is_none());
        assert_events(&[envelope.event], std::slice::from_ref(event));
    }
    let StreamMessage::AgentUpdated { agent: updated } = published.try_recv().unwrap() else {
        panic!("expected synchronized agent snapshot");
    };
    assert_eq!(updated.state, AgentState::Interrupted);
    let StreamMessage::SessionUpdated { session } = published.try_recv().unwrap() else {
        panic!("expected committed session recency snapshot");
    };
    assert_eq!(session.id, agent.session_id);
    assert_eq!(session.updated_at, updated.updated_at);
    assert!(published.try_recv().is_err());
    fixture.close().await;
}

struct TraceTool {
    interrupt: bool,
}

impl Tool for TraceTool {
    fn tool_definition(&self) -> api::ToolDefinition {
        crate::components::tools::tool_definition(psa::api::ToolId::ListSessions).unwrap()
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    crate::components::tools::typed_tool_execute!(TraceToolArgs, execute_typed);
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TraceToolArgs {}

impl TraceTool {
    async fn execute_typed(
        &self,
        _arguments: TraceToolArgs,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        if self.interrupt {
            context
                .save_events(&[
                    log("tool trace, not its response"),
                    status_event(AgentState::Interrupted),
                ])
                .await
                .unwrap();
        }
        ToolOutput::error(crate::prompts::text("model-facing tool error"))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ValidationArgs {
    entries: Vec<ValidationEntry>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ValidationEntry {
    count: u64,
}

struct ValidationTool;

impl ValidationTool {
    async fn execute_typed(
        &self,
        arguments: ValidationArgs,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        assert_eq!(arguments.entries[0].count, 7);
        context
            .save_events(&[log("typed tool executed")])
            .await
            .unwrap();
        ToolOutput::success(Vec::new())
    }
}

impl Tool for ValidationTool {
    fn tool_definition(&self) -> api::ToolDefinition {
        api::ToolDefinition::for_type::<ValidationArgs>(
            "validation",
            "Validate arguments",
            Some(false),
        )
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    crate::components::tools::typed_tool_execute!(ValidationArgs, execute_typed);
}

#[tokio::test]
async fn typed_execute_rejects_bad_fields_without_side_effects_then_delegates_valid_input() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);

    for (arguments, expected) in [
        (serde_json::json!({}), "entries"),
        (serde_json::json!({"entries": [{}]}), "count"),
        (
            serde_json::json!({"entries": [{"count": "SECRET_ARGUMENT"}]}),
            "entries[0].count",
        ),
        (
            serde_json::json!({"entries": [], "extra": "SECRET_ARGUMENT"}),
            "extra",
        ),
        (serde_json::json!([]), "JSON object"),
    ] {
        let (_sender, cancellation) = Cancellation::channel();
        let output = ValidationTool
            .execute(arguments, &mut context, cancellation)
            .await;
        assert!(output.error);
        let [api::ContentBlock::Text(block)] = output.content.as_slice() else {
            panic!("expected a text argument error");
        };
        assert!(block.text.contains(expected), "{}", block.text);
        assert!(!block.text.contains("SECRET_ARGUMENT"));
        assert!(context.load_all_events().await.unwrap().is_empty());
    }

    let (_sender, cancellation) = Cancellation::channel();
    let output = ValidationTool
        .execute(
            serde_json::json!({"entries": [{"count": 7}]}),
            &mut context,
            cancellation,
        )
        .await;
    assert!(!output.error);
    let saved = context.load_all_events().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert!(matches!(&saved[0].kind, EventType::Log(log) if log.message == "typed tool executed"));
    fixture.close().await;
}

#[tokio::test]
async fn typed_execute_checks_cancellation_before_argument_validation() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (sender, cancellation) = Cancellation::channel();
    sender.send(()).unwrap();

    let output = ValidationTool
        .execute(serde_json::json!([]), &mut context, cancellation)
        .await;
    assert!(output.error);
    assert_eq!(
        serde_json::to_value(output.content).unwrap(),
        serde_json::to_value(crate::prompts::tool_interrupted()).unwrap()
    );
    assert!(context.load_all_events().await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn tool_trace_and_status_are_saved_separately_from_worker_response() {
    let fixture = Fixture::new().await;
    let mut events = inference_events(true);
    events.push(status_event(AgentState::Running));
    let agent = fixture.create_agent(&events).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let (_sender, cancellation) = Cancellation::channel();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let output = TraceTool { interrupt: true }
        .execute(serde_json::json!({}), &mut context, cancellation)
        .await;
    assert!(output.error);
    assert_eq!(context.agent().state, AgentState::Interrupted);
    let (_controls, mut controls) = actors::unbounded_channel();
    let mut interrupted = false;
    let skipped = crate::components::tools::execute(
        &[],
        &api::ToolCall {
            call_id: "unstarted-next".into(),
            name: "noop".into(),
            arguments: "{}".into(),
        },
        &mut context,
        &mut interrupted,
        &mut controls,
    )
    .await;
    assert!(skipped.error);
    assert_eq!(
        serde_json::to_value(skipped.content).unwrap(),
        serde_json::to_value(crate::prompts::tool_not_run()).unwrap()
    );
    assert!(!interrupted);
    let before_response = context.load_all_events().await.unwrap();
    assert_eq!(before_response.len(), events.len() + 2);
    assert!(
        before_response
            .iter()
            .all(|event| !matches!(event.kind, EventType::ToolResult(_)))
    );

    state
        .save_tool_output("call-1".into(), output)
        .await
        .unwrap();
    let mut cursor = fixture.database.read().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    cursor.close().await.unwrap();
    assert_eq!(saved.len(), before_response.len() + 1);
    let EventType::ToolResult(result) = &saved.last().unwrap().kind else {
        panic!("worker must persist a separate response");
    };
    assert_eq!(result.call_id, "call-1");
    assert!(result.error);
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::to_value(crate::prompts::text("model-facing tool error")).unwrap()
    );
    assert_eq!(state.context().last_event_id, saved.last().unwrap().id);
    fixture.close().await;
}

struct TraceHook;

impl Hook for TraceHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let agent = context
            .runtime()
            .get_agent(context.session().id, context.agent().id)
            .await?;
        assert_eq!(agent.id, context.agent().id);
        assert_eq!(context.load_all_events().await?.len(), 1);
        context
            .save_events(&[log("hook trace"), status_event(AgentState::Idle)])
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn stop_without_tools_preserves_running_when_interrupt_is_already_queued() {
    let fixture = Fixture::new().await;
    let mut events = vec![status_event(AgentState::Running)];
    events.extend(inference_events(false));
    let agent = fixture.create_agent(&events).await;
    let mut state = fixture.load_state(&agent).await;
    let mut context = AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
    let (sender, mut controls) = actors::unbounded_channel();
    AgentClient::new(sender).interrupt().unwrap();

    assert!(
        run_hook(&mut StopWithoutTools, &mut context, &mut controls)
            .await
            .unwrap()
    );
    assert_eq!(context.agent().state, AgentState::Running);
    assert_eq!(context.projection().status, AgentState::Running);
    assert_eq!(context.load_all_events().await.unwrap().len(), events.len());
    fixture.close().await;
}

#[tokio::test]
async fn stop_without_tools_does_not_complete_when_cancelled_during_trace_read() {
    let fixture = Fixture::new().await;
    let mut events = vec![status_event(AgentState::Running)];
    events.extend(inference_events(false));
    let agent = fixture.create_agent(&events).await;
    let mut state = fixture.load_state(&agent).await;
    let mut context = AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
    let mut hook = StopWithoutTools;
    let (sender, cancellation) = Cancellation::channel();

    // Hold the writer reservation so the hook has checked its token but cannot
    // finish loading the trace before cancellation arrives.
    {
        let cursor = fixture.database.read().await.unwrap();
        let run = hook.run(&mut context, cancellation);
        tokio::pin!(run);
        assert!(futures_util::poll!(&mut run).is_pending());
        sender.send(()).unwrap();
        cursor.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), &mut run)
            .await
            .unwrap()
            .unwrap();
    }

    assert_eq!(context.agent().state, AgentState::Running);
    assert_eq!(context.projection().status, AgentState::Running);
    assert_eq!(context.load_all_events().await.unwrap().len(), events.len());
    fixture.close().await;
}

#[tokio::test]
async fn stop_without_tools_only_completes_answers_without_tool_calls() {
    let fixture = Fixture::new().await;
    for has_tool_call in [false, true] {
        let mut events = vec![status_event(AgentState::Running)];
        events.extend(inference_events(has_tool_call));
        let agent = fixture.create_agent(&events).await;
        let mut state = fixture.load_state(&agent).await;
        let mut context =
            AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
        let (_sender, cancellation) = Cancellation::channel();
        StopWithoutTools
            .run(&mut context, cancellation)
            .await
            .unwrap();

        let expected = if has_tool_call {
            AgentState::Running
        } else {
            AgentState::Idle
        };
        assert_eq!(context.agent().state, expected);
        assert_eq!(context.projection().status, expected);
        let mut cursor = fixture.database.read().await.unwrap();
        assert_eq!(cursor.get_agent(agent.id).await.unwrap().state, expected);
        cursor.close().await.unwrap();
    }
    fixture.close().await;
}

#[tokio::test]
async fn hooks_share_trace_and_session_capabilities_and_save_status() {
    let fixture = Fixture::new().await;
    let agent = fixture
        .create_agent(&[status_event(AgentState::Running)])
        .await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (_controls, mut controls) = actors::unbounded_channel();
    assert!(
        !run_hook(&mut TraceHook, &mut context, &mut controls)
            .await
            .unwrap()
    );
    assert_eq!(context.agent().state, AgentState::Idle);
    assert_eq!(context.projection().status, AgentState::Idle);
    let saved = context.load_all_events().await.unwrap();
    assert_eq!(saved.len(), 3);
    assert!(matches!(&saved[1].kind, EventType::Log(log) if log.message == "hook trace"));
    assert!(
        matches!(&saved[2].kind, EventType::StatusChange(change) if change.new_status == AgentState::Idle)
    );
    fixture.close().await;
}

#[tokio::test]
async fn extra_system_prompt_hook_persists_verbatim_system_message_and_skips_empty() {
    let fixture = Fixture::new().await;
    let seed = fixture.create_agent(&[]).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();

    for prompt in [
        "",
        " \n",
        "  Follow these extra instructions.\n<literal> & text\n",
    ] {
        let mut cursor = fixture.database.write().await.unwrap();
        let definition = cursor
            .create_agent_definition(psa_store::CreateAgentDefinition {
                name: "extra prompt".into(),
                bundles: vec![psa::api::BundleConfig::ExtraSystemPrompt {
                    prompt: prompt.into(),
                }],
                ..Default::default()
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                seed.session_id,
                NewAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    definition_id: definition.id,
                    name: "extra prompt".into(),
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();

        let mut state = fixture.load_state(&agent).await;
        let mut hook = crate::components::hooks::HookKind::ExtraSystemPrompt(
            crate::components::hooks::ExtraSystemPromptHook::new(prompt.into()),
        );
        let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
        let (_controls, mut controls) = actors::unbounded_channel();
        assert!(
            !run_hook(&mut hook, &mut context, &mut controls)
                .await
                .unwrap()
        );

        let saved = context.load_all_events().await.unwrap();
        if prompt.is_empty() {
            assert!(saved.is_empty());
        } else {
            assert_eq!(saved.len(), 1);
            let EventType::Message(message) = &saved[0].kind else {
                panic!("expected a system message");
            };
            assert_eq!(message.message_type, MessageType::System);
            assert_eq!(
                message.description.as_deref(),
                Some("Additional instructions")
            );
            let [api::ContentBlock::Text(block)] = message.content.as_slice() else {
                panic!("expected one text block");
            };
            assert_eq!(block.text, prompt);
        }
    }

    fixture.close().await;
}

#[tokio::test]
async fn extra_system_prompt_hook_skips_forks_which_inherit_it() {
    let fixture = Fixture::new().await;
    let seed = fixture.create_agent(&[]).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut cursor = fixture.database.write().await.unwrap();
    let fork = cursor
        .fork_session(
            seed.session_id,
            psa_store::SessionFork {
                point: psa_store::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let forked = &fork.agents[0];
    assert_eq!(forked.forked_from, Some(seed.id));

    let mut state = fixture.load_state(forked).await;
    let mut hook = crate::components::hooks::HookKind::ExtraSystemPrompt(
        crate::components::hooks::ExtraSystemPromptHook::new("Extra instructions".into()),
    );
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let before = context.load_all_events().await.unwrap().len();
    let (_controls, mut controls) = actors::unbounded_channel();
    assert!(
        !run_hook(&mut hook, &mut context, &mut controls)
            .await
            .unwrap()
    );

    assert_eq!(context.load_all_events().await.unwrap().len(), before);
    fixture.close().await;
}

async fn run_native_file_tool(
    tool: &impl Tool,
    arguments: serde_json::Value,
    context: &mut AgentContext<'_>,
) -> ToolOutput {
    let (_sender, cancellation) = Cancellation::channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(arguments, context, cancellation),
    )
    .await
    .expect("native file tool did not complete")
}

fn native_tool_text(output: &ToolOutput) -> &str {
    let [api::ContentBlock::Text(block)] = output.content.as_slice() else {
        panic!("expected one text result");
    };
    &block.text
}

#[tokio::test]
async fn native_write_read_and_disjoint_edit_use_the_remote_filesystem() {
    let fixture = Fixture::new_with_files(fake_probe::FakeFiles::default()).await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let write = Write;
    let read = Read;
    let edit = Edit;
    let path = "@nested/文件.txt";
    let original = "first\n世界\nthird\nfourth";

    let output = run_native_file_tool(
        &write,
        serde_json::json!({"path": path, "content": original}),
        &mut context,
    )
    .await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(
        native_tool_text(&output),
        "Successfully wrote to @nested/文件.txt"
    );
    let entries = probe.list_dir("/work/nested").await.unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "文件.txt" && entry.size == original.len() as u64)
    );
    assert!(
        probe
            .list_dir("/work")
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.name == "nested" && entry.kind == "directory")
    );

    let output = run_native_file_tool(&read, serde_json::json!({"path": path}), &mut context).await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(native_tool_text(&output), original);
    let output = run_native_file_tool(
        &read,
        serde_json::json!({"path": path, "offset": 2, "limit": 2}),
        &mut context,
    )
    .await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(
        native_tool_text(&output),
        "世界\nthird\n\n[1 more lines in file. Use offset=4 to continue.]"
    );

    let output = run_native_file_tool(
        &edit,
        serde_json::json!({"path": path, "edits": [
            {"oldText": "first", "newText": "FIRST"},
            {"oldText": "fourth", "newText": "last"}
        ]}),
        &mut context,
    )
    .await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(
        native_tool_text(&output),
        "Successfully replaced 2 block(s) in @nested/文件.txt."
    );
    let output = run_native_file_tool(
        &read,
        serde_json::json!({"path": "/work/nested/文件.txt"}),
        &mut context,
    )
    .await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(native_tool_text(&output), "FIRST\n世界\nthird\nlast");

    let output = run_native_file_tool(
        &edit,
        serde_json::json!({"path": path, "edits": [
            {"oldText": "FIRST", "newText": "must not be applied"},
            {"oldText": "does not exist", "newText": "invalid"}
        ]}),
        &mut context,
    )
    .await;
    assert!(output.error);
    let output = run_native_file_tool(&read, serde_json::json!({"path": path}), &mut context).await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(native_tool_text(&output), "FIRST\n世界\nthird\nlast");
    assert!(context.load_all_events().await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn native_file_tools_resolve_each_sessions_cwd_and_overwrite_large_streams() {
    let fixture = Fixture::new_with_files(fake_probe::FakeFiles::default()).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let first = fixture.create_agent_in("/first", &[]).await;
    let second = fixture.create_agent_in("/second", &[]).await;
    for (agent, content) in [(&first, "first contents"), (&second, "second contents")] {
        let mut state = fixture.load_state(agent).await;
        let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
        let write = Write;
        let large = "界\n".repeat(100_000);
        let output = run_native_file_tool(
            &write,
            serde_json::json!({"path": "same.txt", "content": large}),
            &mut context,
        )
        .await;
        assert!(!output.error, "{}", native_tool_text(&output));
        let output = run_native_file_tool(
            &write,
            serde_json::json!({"path": "same.txt", "content": content}),
            &mut context,
        )
        .await;
        assert!(!output.error, "{}", native_tool_text(&output));
    }
    let mut state = fixture.load_state(&first).await;
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    for (path, expected) in [
        ("/first/same.txt", "first contents"),
        ("/second/same.txt", "second contents"),
    ] {
        let output =
            run_native_file_tool(&Read, serde_json::json!({"path": path}), &mut context).await;
        assert!(!output.error, "{}", native_tool_text(&output));
        assert_eq!(native_tool_text(&output), expected);
    }
    fixture.close().await;
}

#[tokio::test]
async fn native_file_tools_reject_invalid_or_cancelled_writes_without_mutation() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default().file("/work/keep.txt", "original"),
    )
    .await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    for arguments in [
        serde_json::json!({"path": "keep.txt"}),
        serde_json::json!({"path": "keep.txt", "content": 3}),
        serde_json::json!({"path": "keep.txt", "content": null}),
    ] {
        assert!(
            run_native_file_tool(&Write, arguments, &mut context)
                .await
                .error
        );
    }
    let (sender, cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    let output = Write
        .execute(
            serde_json::json!({"path": "keep.txt", "content": "changed"}),
            &mut context,
            cancellation,
        )
        .await;
    assert!(output.error);
    assert_eq!(
        serde_json::to_value(output.content).unwrap(),
        serde_json::to_value(crate::prompts::tool_interrupted()).unwrap()
    );
    let output =
        run_native_file_tool(&Read, serde_json::json!({"path": "keep.txt"}), &mut context).await;
    assert!(!output.error, "{}", native_tool_text(&output));
    assert_eq!(native_tool_text(&output), "original");
    let output = run_native_file_tool(
        &Write,
        serde_json::json!({"path": "keep.txt/child", "content": "text"}),
        &mut context,
    )
    .await;
    assert!(output.error);
    fixture.close().await;
}

#[tokio::test]
async fn native_write_cancels_a_pending_remote_directory_creation() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (sender, cancellation) = Cancellation::channel();
    let output = {
        let run = Write.execute(
            serde_json::json!({"path": "file.txt", "content": "text"}),
            &mut context,
            cancellation,
        );
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = &mut run => panic!("inert probe unexpectedly completed write"),
            _ = tokio::task::yield_now() => {}
        }
        sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .unwrap()
    };
    assert!(output.error);
    assert_eq!(
        serde_json::to_value(output.content).unwrap(),
        serde_json::to_value(crate::prompts::tool_interrupted()).unwrap()
    );
    fixture.close().await;
}

async fn run_project_context(fixture: &Fixture, cwd: &str) -> Vec<Event> {
    let agent = fixture.create_agent_in(cwd, &[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut hook = crate::components::hooks::HookKind::ProjectContext(
        crate::components::hooks::ProjectContextHook,
    );
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (_controls, mut controls) = actors::unbounded_channel();
    assert!(
        !run_hook(&mut hook, &mut context, &mut controls)
            .await
            .unwrap()
    );
    context.load_all_events().await.unwrap()
}

fn project_context_prompt(files: &[(&str, &str)]) -> String {
    let mut prompt =
        "<project_context>\nProject-specific instructions and guidelines:\n\n".to_owned();
    for (index, (path, content)) in files.iter().enumerate() {
        if index > 0 {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&format!(
            "<project_instructions path=\"{path}\">\n{content}\n</project_instructions>"
        ));
    }
    prompt.push_str("\n</project_context>");
    prompt
}

fn assert_project_context_message(events: &[Event], files: &[(&str, &str)]) {
    let messages: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventType::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message_type, MessageType::System);
    let [api::ContentBlock::Text(block)] = messages[0].content.as_slice() else {
        panic!("expected one project context text block");
    };
    assert_eq!(block.text, project_context_prompt(files));
}

#[tokio::test]
async fn project_context_orders_ancestors_and_preserves_bom_stripped_content_verbatim() {
    let root = "  root instructions\n<literal> & text\n";
    let work = "\r\nwork instructions\r\n";
    let nested = "\u{feff}second BOM stays";
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default()
            .file("/work/nested/AGENTS.md", format!("\u{feff}{nested}"))
            .file("/work/CLAUDE.MD", work)
            .file("/AGENTS.md", format!("\u{feff}{root}"))
            .file("/work/other/AGENTS.md", "not an ancestor"),
    )
    .await;
    let events = run_project_context(&fixture, "/work/nested").await;
    assert_eq!(events.len(), 1);
    assert_project_context_message(
        &events,
        &[
            ("/AGENTS.md", root),
            ("/work/CLAUDE.MD", work),
            ("/work/nested/AGENTS.md", nested),
        ],
    );
    fixture.close().await;
}

#[tokio::test]
async fn project_context_uses_exact_candidate_priority_in_each_directory() {
    let candidates = [
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ];
    for first in 0..candidates.len() {
        let mut files = fake_probe::FakeFiles::default();
        for name in candidates[first..].iter().rev() {
            files = files.file(&format!("/work/{name}"), *name);
        }
        let fixture = Fixture::new_with_files(files).await;
        let events = run_project_context(&fixture, "/work").await;
        assert_eq!(events.len(), 1);
        assert_project_context_message(
            &events,
            &[(&format!("/work/{}", candidates[first]), candidates[first])],
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn project_context_empty_files_shadow_lower_priority_candidates() {
    for content in ["", "\u{feff}"] {
        let fixture = Fixture::new_with_files(
            fake_probe::FakeFiles::default()
                .file("/work/AGENTS.override.md", content)
                .file("/work/AGENTS.md", "must not appear"),
        )
        .await;
        let events = run_project_context(&fixture, "/work").await;
        assert_eq!(events.len(), 1);
        assert_project_context_message(&events, &[("/work/AGENTS.override.md", "")]);
        fixture.close().await;
    }
}

#[tokio::test]
async fn project_context_no_discovered_files_emits_no_message_or_warning() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default()
            .file("/work/agents.md", "wrong case")
            .file("/work/README.md", "not instructions")
            .directory("/work/AGENTS.md"),
    )
    .await;
    assert!(run_project_context(&fixture, "/work").await.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn project_context_remote_failures_warn_and_fall_back_to_readable_symlink() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default()
            .list_error("/")
            .unreadable("/work/AGENTS.override.md")
            .symlink("/work/AGENTS.md", "linked instructions"),
    )
    .await;
    let events = run_project_context(&fixture, "/work").await;
    assert_project_context_message(&events, &[("/work/AGENTS.md", "linked instructions")]);
    let warnings: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventType::Log(log) => Some(log),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|log| !log.message.is_empty()));
    fixture.close().await;
}

#[tokio::test]
async fn project_context_invalid_utf8_and_oversized_files_warn_and_fall_back() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default()
            .file("/work/AGENTS.override.md", vec![b'x'; 1024 * 1024 + 1])
            .file("/work/AGENTS.md", vec![0xff, 0xfe])
            .file("/work/AGENTS.MD", "valid fallback"),
    )
    .await;
    let events = run_project_context(&fixture, "/work").await;
    assert_project_context_message(&events, &[("/work/AGENTS.MD", "valid fallback")]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventType::Log(_)))
            .count(),
        2
    );
    fixture.close().await;
}

#[tokio::test]
async fn project_context_cancelled_before_discovery_saves_no_partial_prompt() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default()
            .file("/AGENTS.md", "root")
            .file("/work/AGENTS.md", "work"),
    )
    .await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut hook = crate::components::hooks::HookKind::ProjectContext(
        crate::components::hooks::ProjectContextHook,
    );
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (sender, cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    hook.run(&mut context, cancellation).await.unwrap();
    assert!(context.load_all_events().await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn project_context_cancels_a_pending_remote_operation() {
    // The connection-only fake never answers directory requests.
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let mut hook = crate::components::hooks::HookKind::ProjectContext(
        crate::components::hooks::ProjectContextHook,
    );
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let (sender, cancellation) = Cancellation::channel();
    {
        let run = hook.run(&mut context, cancellation);
        tokio::pin!(run);
        tokio::select! {
            biased;
            result = &mut run => panic!("discovery unexpectedly completed: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(context.load_all_events().await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn project_context_accepts_limit_sized_files_and_caps_total_content() {
    let content = "x".repeat(1024 * 1024);
    let paths = [
        "/AGENTS.md",
        "/a/AGENTS.md",
        "/a/b/AGENTS.md",
        "/a/b/c/AGENTS.md",
        "/a/b/c/d/AGENTS.md",
    ];
    let mut files = fake_probe::FakeFiles::default();
    for path in paths {
        files = files.file(path, content.as_bytes().to_vec());
    }
    let fixture = Fixture::new_with_files(files).await;
    let events = run_project_context(&fixture, "/a/b/c/d").await;
    let expected: Vec<_> = paths[..4]
        .iter()
        .map(|path| (*path, content.as_str()))
        .collect();
    assert_project_context_message(&events, &expected);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventType::Log(_)))
            .count(),
        1
    );
    fixture.close().await;
}

#[tokio::test]
async fn project_context_disconnected_probe_is_fatal_and_saves_no_prompt() {
    let fixture = Fixture::new_with_files(
        fake_probe::FakeFiles::default().file("/work/AGENTS.md", "must not be saved"),
    )
    .await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let probe_handle = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let Fixture {
        _directory,
        database,
        publication: _,
        client: _,
        base: _,
        runtime,
        probes,
        probe,
        stop,
        server,
    } = fixture;
    probe.disconnect(&probes).await;
    let mut hook = crate::components::hooks::HookKind::ProjectContext(
        crate::components::hooks::ProjectContextHook,
    );
    let mut context = AgentContext::new(&mut state, &probe_handle, &runtime.handle);
    let (_sender, cancellation) = Cancellation::channel();
    assert!(hook.run(&mut context, cancellation).await.is_err());
    assert!(context.load_all_events().await.unwrap().is_empty());
    runtime.shutdown().await.unwrap();
    probes.shutdown().await;
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    database.close().await;
    drop(_directory);
}

#[tokio::test]
async fn model_facing_tool_error_does_not_change_running_status() {
    let fixture = Fixture::new().await;
    let mut events = inference_events(true);
    events.push(status_event(AgentState::Running));
    let agent = fixture.create_agent(&events).await;
    let mut state = fixture.load_state(&agent).await;
    let probe = fixture
        .probes
        .handle(support::probe_metadata().id)
        .await
        .unwrap();
    let (_sender, cancellation) = Cancellation::channel();
    let mut context = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    let output = TraceTool { interrupt: false }
        .execute(serde_json::json!({}), &mut context, cancellation)
        .await;
    assert!(output.error);
    assert_eq!(context.agent().state, AgentState::Running);
    assert_eq!(context.load_all_events().await.unwrap().len(), events.len());
    state
        .save_tool_output("call-1".into(), output)
        .await
        .unwrap();
    assert_eq!(state.agent().state, AgentState::Running);
    assert_eq!(state.context().status, AgentState::Running);
    let mut cursor = fixture.database.read().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(saved.len(), events.len() + 1);
    assert!(matches!(&saved.last().unwrap().kind, EventType::ToolResult(result) if result.error));
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );
    cursor.close().await.unwrap();
    fixture.close().await;
}
