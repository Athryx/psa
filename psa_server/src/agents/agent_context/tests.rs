use super::*;
use crate::{
    agents::{
        AgentState, NewAgent,
        publication::{Publication, status_event},
        session_manager::Runtime,
    },
    hooks::{
        Hook, HookError,
        cancellation::Cancellation,
        run_hook,
        tools::{Tool, ToolOutput},
    },
    http::AppState,
    probes::ProbeRegistry,
    providers::ProviderRegistry,
    sessions::{ForkPoint, NewSession, SessionFork},
    test_support as support,
};
use llm_api::{
    api::{self, Outcome, StopReason},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::StreamMessage;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

#[path = "../../../tests/support/fake_probe.rs"]
mod fake_probe;

struct Fixture {
    _directory: tempfile::TempDir,
    database: crate::db::Database,
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
        let database = crate::db::Database::open(directory.path().join("context.sqlite3"))
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

    async fn create_agent(&self, events: &[NewEvent]) -> Agent {
        let mut cursor = self.database.cursor().await.unwrap();
        let session = cursor
            .create_session(NewSession {
                model_config: crate::test_support::model_config(),
                name: "context test".into(),
                working_directory: "/work".into(),
                probe_id: support::probe_metadata().id,
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                session.id,
                NewAgent {
                    interactive: false,
                    definition_id: psa_api::AgentDefinitionId(1),
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
        model: Model::new("test-model").unwrap(),
        provider: ProviderType::Openrouter,
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
    let mut cursor = fixture.database.cursor().await.unwrap();
    let other = cursor
        .create_agent(
            parent.session_id,
            NewAgent {
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
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
        .find(|(id, _)| *id == parent.id)
        .unwrap()
        .1
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
            .sessions()
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
    assert!(published.try_recv().is_err());
    fixture.close().await;
}

struct TraceTool {
    interrupt: bool,
}

impl Tool for TraceTool {
    fn new(_agent: &Agent, _session: &crate::sessions::Session) -> Self {
        Self { interrupt: false }
    }

    fn tool_definition(&self) -> api::ToolDefinition {
        crate::hooks::tools::tool_definition(psa_api::ToolId::Noop)
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
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
    let skipped = crate::hooks::tools::execute(
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
    let mut cursor = fixture.database.cursor().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    cursor.rollback().await.unwrap();
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
    fn new(_agent: &Agent, _session: &crate::sessions::Session) -> Self {
        Self
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let agent = context
            .sessions()
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
        let mut cursor = fixture.database.cursor().await.unwrap();
        let definition = cursor
            .create_agent_definition(psa_api::CreateAgentDefinition {
                name: "extra prompt".into(),
                extra_system_prompt: prompt.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                seed.session_id,
                NewAgent {
                    interactive: false,
                    definition_id: definition.id,
                    name: "extra prompt".into(),
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();

        let mut state = fixture.load_state(&agent).await;
        let mut hook = crate::hooks::HookKind::new(
            psa_api::HookId::ExtraSystemPrompt,
            state.agent(),
            state.session(),
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
            let [api::ContentBlock::Text(block)] = message.content.as_slice() else {
                panic!("expected one text block");
            };
            assert_eq!(block.text, prompt);
        }
    }

    fixture.close().await;
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
    let mut cursor = fixture.database.cursor().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(saved.len(), events.len() + 1);
    assert!(matches!(&saved.last().unwrap().kind, EventType::ToolResult(result) if result.error));
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );
    cursor.rollback().await.unwrap();
    fixture.close().await;
}
