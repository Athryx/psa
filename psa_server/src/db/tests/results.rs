use super::*;
use psa_api::AgentResult;

fn completion(value: AgentResult) -> [NewEvent; 3] {
    [
        state_change(AgentState::Running),
        NewEvent::new(EventType::Result(value)),
        state_change(AgentState::Idle),
    ]
}

#[tokio::test]
async fn result_history_is_newest_first_durable_and_bounded_by_every_fork() {
    let (directory, db, session, parent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    assert!(
        cursor
            .load_last_result(session.id, parent.id)
            .await
            .unwrap()
            .is_none()
    );
    let first = cursor
        .add_events(
            parent.id,
            &[
                state_change(AgentState::Running),
                log("not a result"),
                NewEvent::new(EventType::Result(AgentResult::Text("first".into()))),
                state_change(AgentState::Idle),
            ],
        )
        .await
        .unwrap();
    let first_id = first.events[2].id;
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: parent.id,
                    event_id: first_id,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0];
    assert_eq!(
        child.state,
        AgentState::Idle,
        "a Result fork point is logically terminal before its paired Idle event"
    );
    cursor
        .add_events(
            parent.id,
            &completion(AgentResult::Json(
                serde_json::json!({"private_parent":true}),
            )),
        )
        .await
        .unwrap();
    let child_saved = cursor
        .add_events(child.id, &completion(AgentResult::Text("child".into())))
        .await
        .unwrap();
    let child_result_id = child_saved.events[1].id;
    let nested = cursor
        .fork_session(
            fork.session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: child.id,
                    event_id: child_result_id,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let descendant = &nested.agents[0];
    assert_eq!(descendant.state, AgentState::Idle);
    cursor
        .add_events(
            child.id,
            &completion(AgentResult::Text("private child continuation".into())),
        )
        .await
        .unwrap();
    cursor
        .add_events(
            descendant.id,
            &completion(AgentResult::Json(serde_json::json!({"answer":42}))),
        )
        .await
        .unwrap();

    let results = cursor
        .load_last_results(nested.session.id, descendant.id, 10)
        .await
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|record| record.value.clone())
            .collect::<Vec<_>>(),
        vec![
            AgentResult::Json(serde_json::json!({"answer":42})),
            AgentResult::Text("child".into()),
            AgentResult::Text("first".into()),
        ]
    );
    assert_eq!(
        results
            .iter()
            .map(|record| record.source_agent_id)
            .collect::<Vec<_>>(),
        vec![descendant.id, child.id, parent.id]
    );
    assert!(
        results.iter().all(
            |record| record.session_id == nested.session.id && record.agent_id == descendant.id
        )
    );
    assert!(
        results
            .windows(2)
            .all(|pair| pair[0].event_id > pair[1].event_id)
    );
    assert_eq!(
        cursor
            .load_last_result(nested.session.id, descendant.id)
            .await
            .unwrap(),
        Some(results[0].clone())
    );
    assert_eq!(
        cursor
            .load_last_results(nested.session.id, descendant.id, 2)
            .await
            .unwrap(),
        results[..2]
    );
    for limit in [0, 10001] {
        assert!(
            cursor
                .load_last_results(nested.session.id, descendant.id, limit)
                .await
                .is_err()
        );
    }
    assert!(
        cursor
            .load_last_result(session.id, descendant.id)
            .await
            .is_err()
    );

    // Limits apply after filtering, and pagination cursors remain full transcript IDs.
    let mut after = EventId(0);
    let mut filtered = Vec::new();
    loop {
        let page = cursor
            .load_agent_result_envelopes(
                nested.session.id,
                descendant.id,
                EventRange {
                    after,
                    limit: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        assert!(matches!(page[0].event.kind, EventType::Result(_)));
        after = page[0].event.id;
        filtered.push(after);
    }
    assert_eq!(
        filtered,
        results
            .iter()
            .rev()
            .map(|record| record.event_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(filtered[0], first_id);
    let bounded = cursor
        .load_agent_result_envelopes(
            nested.session.id,
            descendant.id,
            EventRange {
                through: Some(child_result_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(bounded.len(), 2);
    let session_results = cursor
        .load_session_result_events(nested.session.id, 0, None, 10)
        .await
        .unwrap();
    assert_eq!(session_results.len(), 3);
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.cursor().await.unwrap();
    assert_eq!(
        cursor
            .load_last_results(nested.session.id, descendant.id, 10)
            .await
            .unwrap(),
        results
    );
    assert_eq!(
        cursor.get_agent(descendant.id).await.unwrap().state,
        AgentState::Idle
    );
    cursor.rollback().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn recovery_of_a_structured_result_fork_acknowledges_only_its_producer_and_preserves_idle() {
    use crate::{
        agents::{
            agent_manager::{AgentManagerState, recover_agent},
            publication::Publication,
        },
        components::bundles::InstalledComponents,
    };
    for (previous_run, producer_ack_visible, incomplete_later_call) in [
        (false, false, false),
        (false, true, false),
        (false, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let (_directory, db, session, _ordinary_agent) = fixture().await;
        let mut cursor = db.cursor().await.unwrap();
        let schema = serde_json::json!({
            "type": "object", "properties": {"count": {"type": "integer"}},
            "required": ["count"], "additionalProperties": false,
        });
        let definition = cursor
            .create_agent_definition(psa_api::CreateAgentDefinition {
                name: "typed checkpoint".into(),
                bundles: vec![
                    psa_api::BundleConfig::TypedResult {
                        schema: schema.into(),
                        max_retries: 3,
                    },
                    psa_api::BundleConfig::Noop,
                ],
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
        let record_metadata = psa_api::events::ToolDefinition {
            id: ToolId::RecordResult,
            definition: record_tool.tool_definition(),
        };
        components.cleanup().await.unwrap();
        let mut cursor = db.cursor().await.unwrap();
        let ack = |call_id: &str| {
            NewEvent::new(EventType::ToolResult(ToolResult {
                call_id: call_id.into(),
                content: crate::prompts::text("Result recorded."),
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
                ToolDefinition::for_tool(ToolId::Noop).unwrap(),
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
                    item_status: incomplete_later_call.then(|| "incomplete".into()),
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
            .find(|child| child.parent_id == Some(agent.id))
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
        let mut cursor = db.cursor().await.unwrap();
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
        cursor.rollback().await.unwrap();
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
        let mut cursor = db.cursor().await.unwrap();
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

#[tokio::test]
async fn results_require_running_and_an_immediate_idle_transition_in_one_batch() {
    let (_directory, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    for invalid in [
        vec![
            NewEvent::new(EventType::Result(AgentResult::Text("not running".into()))),
            state_change(AgentState::Idle),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("missing idle".into()))),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("interleaved".into()))),
            log("between result and idle"),
            state_change(AgentState::Idle),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("first".into()))),
            state_change(AgentState::Idle),
            NewEvent::new(EventType::Result(AgentResult::Text("duplicate".into()))),
            state_change(AgentState::Idle),
        ],
    ] {
        assert!(cursor.add_events(agent.id, &invalid).await.is_err());
        assert!(cursor.load_all_events(agent.id).await.unwrap().is_empty());
        assert_eq!(
            cursor.get_agent(agent.id).await.unwrap().state,
            AgentState::Idle
        );
        assert!(
            cursor
                .load_last_result(session.id, agent.id)
                .await
                .unwrap()
                .is_none()
        );
    }
    cursor
        .add_events(
            agent.id,
            &completion(AgentResult::Json(serde_json::json!({"valid":true}))),
        )
        .await
        .unwrap();
    assert_eq!(
        cursor
            .load_last_results(session.id, agent.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}
