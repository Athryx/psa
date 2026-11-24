//! Recovery of agents whose durable history ends inside a structured result.

use super::{AgentManagerState, recover_agent};
use crate::agents::{ToolDefinitionExt, publication::Publication};
use crate::components::bundles::InstalledComponents;
use llm_api::api::{self as api, Outcome, StopReason};
use psa::api::{AgentResult, AgentState, ToolId, events::*};
use psa_store::{
    ForkPoint, NewAgent, NewEvent, SessionFork, test_support::session_fixture as fixture,
};
use std::time::Duration;

fn model() -> llm_api::providers::ModelConfig {
    psa_store::test_support::model_config()
}

fn state_change(state: AgentState) -> NewEvent {
    NewEvent::new(EventType::StatusChange(StatusChange {
        new_status: state,
        reason: None,
    }))
}

#[tokio::test]
async fn recovery_of_a_structured_result_fork_acknowledges_only_its_producer_and_preserves_idle() {
    for (previous_run, producer_ack_visible, incomplete_later_call) in [
        (false, false, false),
        (false, true, false),
        (false, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let (_directory, db, session, _ordinary_agent) = fixture().await;
        let mut cursor = db.write().await.unwrap();
        let schema = serde_json::json!({
            "type": "object", "properties": {"count": {"type": "integer"}},
            "required": ["count"], "additionalProperties": false,
        });
        let definition = cursor
            .create_agent_definition(psa_store::CreateAgentDefinition {
                name: "typed checkpoint".into(),
                bundles: vec![psa::api::BundleConfig::TypedResult {
                    schema: schema.into(),
                    max_retries: 3,
                }],
                ..Default::default()
            })
            .await
            .unwrap();
        let agent = cursor
            .create_agent(
                session.id,
                NewAgent {
                    name: "typed producer".into(),
                    definition_id: definition.id,
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();

        // Metadata comes from the installed schema-bearing tool, never the static inventory.
        let mut components = InstalledComponents::install(&definition, &agent, &session)
            .await
            .unwrap();
        let record_tool = components
            .tools
            .iter()
            .find(|tool| tool.id() == ToolId::RecordResult)
            .unwrap();
        let record_metadata = psa::api::events::ToolDefinition {
            id: ToolId::RecordResult,
            definition: record_tool.tool_definition(),
        };
        components.cleanup().await.unwrap();
        let mut cursor = db.write().await.unwrap();
        let ack = |call_id: &str| {
            NewEvent::new(EventType::ToolResult(ToolResult {
                delivery: None,
                call_id: call_id.into(),
                content: vec![llm_api::api::ContentBlock::Text(llm_api::api::TextBlock {
                    text: "Result recorded.".into(),
                    annotations: Vec::new(),
                })],
                error: false,
                metadata: Default::default(),
            }))
        };
        let inference_end = |id| {
            NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                id,
                outcome: Outcome::Completed {
                    reason: StopReason::ToolCalls,
                },
                usage: Default::default(),
                request_time: Duration::ZERO,
                response_id: None,
                provider_metadata: Default::default(),
            }))
        };
        let mut checkpoint = vec![
            NewEvent::new(EventType::Model(model())),
            NewEvent::new(EventType::ToolDefinition(record_metadata)),
            NewEvent::new(EventType::ToolDefinition(
                <ToolDefinition as ToolDefinitionExt>::for_tool(ToolId::ListSessions).unwrap(),
            )),
        ];
        if previous_run {
            checkpoint.extend([
                state_change(AgentState::Running),
                NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                    id: 0,
                    model_config: model(),
                    request_config: Default::default(),
                })),
                NewEvent::new(EventType::ToolCall(ToolCall {
                    call_id: "previous-producer".into(),
                    tool_name: "record_result".into(),
                    arguments: r#"{"count":1}"#.into(),
                    metadata: Default::default(),
                })),
                inference_end(0),
                NewEvent::new(EventType::Result(AgentResult::Json(
                    serde_json::json!({"count":1}),
                ))),
                state_change(AgentState::Idle),
                ack("previous-producer"),
            ]);
        }
        checkpoint.extend([
            state_change(AgentState::Running),
            NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                id: 1,
                model_config: model(),
                request_config: Default::default(),
            })),
            NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: "producer".into(),
                tool_name: "record_result".into(),
                arguments: r#"{"count":4}"#.into(),
                metadata: Default::default(),
            })),
            NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: "later-noop".into(),
                tool_name: "noop".into(),
                arguments: "{}".into(),
                metadata: Default::default(),
            })),
            NewEvent::new(EventType::ToolCall(ToolCall {
                call_id: "later-result".into(),
                tool_name: "record_result".into(),
                arguments: if incomplete_later_call {
                    r#"{"count":"#.into()
                } else {
                    r#"{"count":9}"#.into()
                },
                metadata: api::HistoryItemMetadata {
                    item_status: incomplete_later_call
                        .then(|| llm_api::api::ItemStatus::Incomplete),
                    ..Default::default()
                },
            })),
            inference_end(1),
            NewEvent::new(EventType::Result(AgentResult::Json(
                serde_json::json!({"count":4}),
            ))),
            state_change(AgentState::Idle),
        ]);
        if producer_ack_visible {
            checkpoint.push(ack("producer"));
        }
        let saved = cursor.add_events(agent.id, &checkpoint).await.unwrap();
        let result_id = saved
            .events
            .iter()
            .rev()
            .find(|event| matches!(event.kind, EventType::Result(_)))
            .unwrap()
            .id;
        let checkpoint_id = if producer_ack_visible {
            saved.events.iter().find_map(|event| {
                matches!(&event.kind, EventType::ToolResult(result) if result.call_id == "producer").then_some(event.id)
            }).unwrap()
        } else {
            result_id
        };
        let fork = cursor
            .fork_session(
                session.id,
                SessionFork {
                    point: ForkPoint::ThroughEvent {
                        agent_id: agent.id,
                        event_id: checkpoint_id,
                    },
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        let child = fork
            .agents
            .iter()
            .find(|child| child.forked_from == Some(agent.id))
            .unwrap();
        assert_eq!(child.state, AgentState::Idle);
        let visible = cursor.load_all_events(child.id).await.unwrap();
        assert_eq!(
            visible
                .iter()
                .filter(|event| matches!(event.kind, EventType::ToolResult(_)))
                .count(),
            usize::from(previous_run) + usize::from(producer_ack_visible)
        );
        cursor.commit().await.unwrap();

        let publication = Publication::default();
        recover_agent(&db, &publication, child.id).await.unwrap();
        let mut cursor = db.read().await.unwrap();
        let recovered = cursor.load_all_events(child.id).await.unwrap();
        assert_eq!(
            cursor.get_agent(child.id).await.unwrap().state,
            AgentState::Idle
        );
        assert_eq!(
            recovered
                .iter()
                .filter(|event| matches!(event.kind, EventType::Result(_)))
                .count(),
            1 + usize::from(previous_run)
        );
        let acknowledgements = recovered
            .iter()
            .filter_map(|event| match &event.kind {
                EventType::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(acknowledgements.len(), 3 + usize::from(previous_run));
        let producer_acks = acknowledgements
            .iter()
            .filter(|result| result.call_id == "producer")
            .collect::<Vec<_>>();
        assert_eq!(producer_acks.len(), 1);
        assert!(!producer_acks[0].error);
        if previous_run {
            assert!(
                acknowledgements
                    .iter()
                    .any(|result| result.call_id == "previous-producer" && !result.error)
            );
        }
        for call_id in ["later-noop", "later-result"] {
            let result = acknowledgements
                .iter()
                .find(|result| result.call_id == call_id)
                .unwrap();
            assert!(result.error);
            assert!(
                serde_json::to_string(&result.content)
                    .unwrap()
                    .to_lowercase()
                    .contains("not run")
            );
        }
        let records = cursor
            .load_last_results(fork.session.id, child.id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1 + usize::from(previous_run));
        if previous_run {
            assert_eq!(
                records[1].value,
                AgentResult::Json(serde_json::json!({"count":1}))
            );
        }
        assert_eq!(records[0].event_id, result_id);
        assert_eq!(records[0].source_agent_id, agent.id);
        assert_eq!(
            records[0].value,
            AgentResult::Json(serde_json::json!({"count":4}))
        );
        cursor.close().await.unwrap();
        let state = AgentManagerState::load_persisted(db.clone(), publication.clone(), child.id)
            .await
            .unwrap();
        for item in state.context().items() {
            if let api::HistoryItemKind::ToolCall(call) = &item.kind {
                assert!(
                    state
                        .context()
                        .items()
                        .iter()
                        .any(|item| matches!(&item.kind,
                api::HistoryItemKind::ToolResult(result) if result.call_id == call.call_id))
                );
            }
        }
        assert_eq!(state.agent().state, AgentState::Idle);

        // Retrying recovery only acknowledges the checkpoint once and never records it again.
        recover_agent(&db, &publication, child.id).await.unwrap();
        let mut cursor = db.write().await.unwrap();
        assert_eq!(
            cursor.load_all_events(child.id).await.unwrap().len(),
            recovered.len()
        );
        assert_eq!(
            cursor
                .load_last_results(fork.session.id, child.id, 10)
                .await
                .unwrap(),
            records
        );
        cursor.rollback().await.unwrap();
        db.close().await;
    }
}
