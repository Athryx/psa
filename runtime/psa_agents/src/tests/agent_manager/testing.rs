//! A worker state over a fresh database and a connected fake probe, for
//! testing tool execution.

use super::{AgentManagerMessage, AgentManagerState};
use crate::{
    AgentError, AgentState, NewAgent,
    factory::ComponentFactory,
    probes::{
        ProbeRegistry,
        fake_probe::{FakeProbe, PROBE_KEY},
    },
    publication::{Publication, status_event},
    supervisor::Runtime,
    test_support as support,
    tools::DynTool,
};
use llm_api::api::{self, Outcome};
use psa::api::events::*;
use psa_models::ProviderRegistry;
use psa_store::{Database, NewEvent, NewSession};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;

pub struct Fixture {
    pub directory: tempfile::TempDir,
    pub database: Database,
    pub publication: Publication,
    pub runtime: Runtime,
    pub probes: ProbeRegistry,
    pub probe: FakeProbe,
}

impl Fixture {
    /// A runtime installing no concrete components.
    pub async fn new() -> Self {
        Self::with_components(Arc::new(crate::bundles::testing::TestComponents)).await
    }

    pub async fn with_components(components: Arc<dyn ComponentFactory>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("parallel.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&database).await;
        let publication = Publication::default();
        let probes = ProbeRegistry::start(PROBE_KEY.into());
        let runtime = Runtime::start(
            database.clone(),
            publication.clone(),
            ProviderRegistry::default(),
            probes.clone(),
            components,
        )
        .await
        .unwrap();
        let probe = FakeProbe::attach(&probes).await;
        Self {
            directory,
            database,
            publication,
            runtime,
            probes,
            probe,
        }
    }

    pub async fn state(
        &self,
        tools: Vec<Box<dyn DynTool>>,
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

    pub async fn run(
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

    pub async fn close(self) {
        self.probe.disconnect(&self.probes).await;
        self.runtime.shutdown().await.unwrap();
        self.probes.shutdown().await;
        self.database.close().await;
    }
}

/// Await a [`Fixture::run`], requiring it to resolve every call.
pub async fn finish(
    run: JoinHandle<(AgentManagerState, Result<(), AgentError>)>,
) -> AgentManagerState {
    let (state, result) = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    assert!(state.outstanding_calls().is_empty());
    state
}

/// Persist one completed inference that made a single tool call.
pub async fn append_tool_call(
    state: &mut AgentManagerState,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) {
    let inference_id = state.agent().next_event_id.0 as u64;
    state
        .persist_events(&[
            NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                id: inference_id,
                model_config: support::model_config(),
                request_config: Default::default(),
            })),
            NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: id.into(),
                tool_name: name.into(),
                arguments: arguments.to_string(),
                metadata: Default::default(),
            })),
            NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                id: inference_id,
                outcome: Outcome::Completed {
                    reason: api::StopReason::ToolCalls,
                },
                usage: Default::default(),
                request_time: Duration::ZERO,
                response_id: None,
                provider_metadata: Default::default(),
            })),
        ])
        .await
        .unwrap();
}

/// The persisted result of tool call `id`: its error flag and joined text.
pub async fn load_tool_result(
    fixture: &Fixture,
    state: &AgentManagerState,
    id: &str,
) -> (bool, String) {
    let mut cursor = fixture.database.read().await.unwrap();
    let events = cursor.load_all_events(state.agent().id).await.unwrap();
    cursor.close().await.unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == id => Some(result),
            _ => None,
        })
        .expect("tool result persisted");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            llm_api::api::ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (result.error, text)
}
