use super::{Database, DbError};
use crate::{
    agents::AgentState,
    agents::{Agent, NewAgent, ToolDefinitionExt},
    components::tools::ToolId,
    sessions::*,
};
use llm_api::{
    api::{self, HistoryItem, HistoryItemKind, HistoryItemMetadata, Outcome, StopReason, Usage},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::events::*;
use sqlx::SqlSafeStr;
use std::time::Duration;

mod cursor;
mod definitions;
mod file_tools;
mod interactive;
mod models;
mod subagents;
mod usage;

async fn fixture() -> (tempfile::TempDir, Database, Session, Agent) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
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
                definition_id: psa_api::AgentDefinitionId(1),
                name: "main".into(),
            },
        )
        .await
        .unwrap();
    let session = cursor.get_session(session.id).await.unwrap();
    cursor.commit().await.unwrap();
    (dir, db, session, agent)
}

fn log(value: &str) -> NewEvent {
    NewEvent::new(EventType::Log(Log {
        error: false,
        message: value.into(),
    }))
}

fn state_change(state: AgentState) -> NewEvent {
    NewEvent::new(EventType::StatusChange(StatusChange {
        new_status: state,
        reason: None,
    }))
}

fn model() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::High,
    }
}
fn usage() -> Usage {
    Usage {
        input_tokens: Some(0),
        output_tokens: Some(7),
        total_tokens: None,
        cached_input_tokens: Some(0),
        reasoning_output_tokens: Some(3),
        provider_details: std::collections::BTreeMap::from([(
            "cache_write".into(),
            serde_json::json!(2),
        )]),
    }
}
fn native() -> HistoryItem {
    HistoryItem {
        kind: HistoryItemKind::ProviderItem(api::ProviderItem {
            provider: ProviderType::Openrouter,
            item_type: "test".into(),
            raw: serde_json::json!({"type":"test", "secret":"opaque"}),
        }),
        metadata: HistoryItemMetadata {
            item_id: Some("native_1".into()),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn round_trips_all_payloads_and_reopens_database() {
    let (dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let mut kinds = vec![
        EventType::Model(model()),
        EventType::NewContext,
        EventType::Thinking(Thinking {
            contents: api::Reasoning {
                summary: vec!["summary".into()],
                opaque_state: Some(api::ProviderOpaqueData {
                    provider: ProviderType::Openrouter,
                    data: serde_json::json!({"encrypted":"x"}),
                }),
            },
            metadata: HistoryItemMetadata::default(),
        }),
        EventType::ProviderItem(native()),
        EventType::ToolDefinition(ToolDefinition::for_tool(ToolId::Noop)),
        EventType::ToolCall(ToolCall {
            call_id: "call".into(),
            tool_name: "noop".into(),
            arguments: "{ malformed".into(),
            metadata: HistoryItemMetadata {
                item_status: Some("incomplete".into()),
                ..Default::default()
            },
        }),
        EventType::ToolResult(ToolResult {
            call_id: "call".into(),
            content: vec![
                api::ContentBlock::Text(api::TextBlock {
                    text: "failure".into(),
                    annotations: vec![],
                }),
                api::ContentBlock::Image(api::ImageBlock {
                    source: api::MediaSource::Url("https://example.test/output.png".into()),
                    detail: Some(api::ImageDetail::High),
                }),
            ],
            error: true,
            metadata: Default::default(),
        }),
        EventType::ToolResult(ToolResult {
            call_id: "empty".into(),
            content: vec![],
            error: false,
            metadata: Default::default(),
        }),
        EventType::Compaction(Compaction {
            summary: CompactionSummary::Text("summary".into()),
            usage: usage(),
        }),
        EventType::Compaction(Compaction {
            summary: CompactionSummary::ProviderItem(native()),
            usage: usage(),
        }),
        EventType::InferenceResultStart(InferenceResultStart {
            id: 4,
            model_config: model(),
            request_config: api::RequestConfig {
                reasoning: None,
                max_output_tokens: Some(100),
                tool_choice: Some(api::ToolChoice::Function {
                    name: "noop".into(),
                }),
                parallel_tool_calls: Some(false),
            },
        }),
        EventType::Log(Log {
            error: true,
            message: "diagnostic".into(),
        }),
    ];
    for status in [
        AgentState::Idle,
        AgentState::Running,
        AgentState::Interrupted,
    ] {
        kinds.push(EventType::StatusChange(StatusChange {
            new_status: status,
            reason: Some("reason".into()),
        }));
    }
    for message_type in [
        MessageType::Human,
        MessageType::Agent,
        MessageType::Harness,
        MessageType::System,
    ]
    .into_iter()
    {
        kinds.push(EventType::Message(Message {
            source: None,
            message_type,
            content: vec![api::ContentBlock::Refusal(api::RefusalBlock {
                text: "refused".into(),
            })],
            metadata: HistoryItemMetadata {
                item_id: Some("message".into()),
                ..Default::default()
            },
        }));
    }
    kinds.push(EventType::Message(Message {
        source: None,
        message_type: MessageType::Human,
        content: vec![],
        metadata: Default::default(),
    }));
    kinds.push(EventType::Message(Message::text(
        MessageType::Human,
        "hello",
    )));
    for outcome in [
        Outcome::Completed {
            reason: StopReason::ToolCalls,
        },
        Outcome::Incomplete {
            reason: StopReason::TokenLimit,
        },
        Outcome::Cancelled,
        Outcome::Failed(llm_api::Error::Transport {
            message: "offline".into(),
        }),
    ] {
        kinds.push(EventType::InferenceResultEnd(InferenceResultEnd {
            id: 4,
            outcome,
            usage: usage(),
            request_time: Duration::from_micros(123),
            response_id: Some("response".into()),
            provider_metadata: std::collections::BTreeMap::from([(
                "extra".into(),
                serde_json::json!(true),
            )]),
        }));
    }
    let events: Vec<_> = kinds.into_iter().map(NewEvent::new).collect();
    let saved = cursor.add_events(agent.id, &events).await.unwrap();
    for event in &saved.events {
        assert_eq!(event.timestamp.unix_timestamp_nanos() % 1000, 0);
    }
    cursor.commit().await.unwrap();
    db.close().await;
    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let loaded = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    assert_eq!(loaded.len(), events.len());
    for ((loaded, saved), original) in loaded.iter().zip(&saved.events).zip(&events) {
        assert_eq!(format!("{:?}", loaded.kind), format!("{:?}", original.kind));
        assert_eq!(loaded.timestamp, saved.timestamp);
        assert_eq!(loaded.id, saved.id);
        assert_eq!(loaded.source_agent_id, agent.id);
    }
    let updated = cursor
        .update_session(
            session.id,
            SessionUpdate {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.working_directory, session.working_directory);
    assert_eq!(cursor.list_sessions(None).await.unwrap().len(), 1);
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn failed_batch_rolls_back_payloads_and_both_allocators() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let before = cursor.get_session(session.id).await.unwrap().next_position;
    let bad = NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
        id: 1,
        outcome: Outcome::InProgress,
        usage: Usage::default(),
        request_time: Duration::ZERO,
        response_id: None,
        provider_metadata: Default::default(),
    }));
    assert!(matches!(
        cursor
            .add_events(
                agent.id,
                &[state_change(AgentState::Running), log("rolled back"), bad]
            )
            .await,
        Err(DbError::InvalidInput(_))
    ));
    assert!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cursor.get_session(session.id).await.unwrap().next_position,
        before
    );
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().next_event_id,
        EventId(1)
    );
    let counts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(counts, 0);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Idle
    );
    let saved = cursor
        .add_events(agent.id, &[log("committed")])
        .await
        .unwrap();
    assert_eq!(saved.events[0].id, EventId(1));
    cursor.add_events(agent.id, &[]).await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().next_event_id,
        EventId(2)
    );
    assert!(
        cursor
            .add_events(crate::agents::AgentId(999), &[])
            .await
            .is_err()
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn event_batches_update_the_current_agent_state() {
    let (dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(agent.state, AgentState::Idle);
    let result: super::AddEventResult = cursor
        .add_events(
            agent.id,
            &[
                state_change(AgentState::Running),
                state_change(AgentState::Idle),
                state_change(AgentState::Interrupted),
                log("after state changes"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(result.new_state, Some(AgentState::Interrupted));
    assert_eq!(result.events.len(), 4);
    assert_eq!(result.events[0].id, EventId(1));
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    assert_eq!(
        cursor.list_agents(session.id).await.unwrap()[0].state,
        AgentState::Interrupted
    );

    let result = cursor
        .add_events(agent.id, &[NewEvent::new(EventType::NewContext)])
        .await
        .unwrap();
    assert_eq!(result.new_state, None);
    assert_eq!(result.events.len(), 1);
    let result = cursor.add_events(agent.id, &[]).await.unwrap();
    assert_eq!(result.new_state, None);
    assert!(result.events.is_empty());
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    let result = cursor
        .add_events(
            agent.id,
            &[state_change(AgentState::Interrupted), log("unchanged")],
        )
        .await
        .unwrap();
    assert_eq!(result.new_state, None);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    let result = cursor
        .add_events(
            agent.id,
            &[
                state_change(AgentState::Running),
                state_change(AgentState::Interrupted),
            ],
        )
        .await
        .unwrap();
    assert_eq!(result.new_state, None);
    assert_eq!(result.events.len(), 2);
    let result = cursor
        .add_events(agent.id, &[state_change(AgentState::Running)])
        .await
        .unwrap();
    assert_eq!(result.new_state, Some(AgentState::Running));
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );

    // A failure after the agent row update must roll back the state and events.
    sqlx::raw_sql("CREATE TRIGGER reject_session_update BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'test rollback'); END;")
        .execute(cursor.connection()).await.unwrap();
    let before = cursor.get_agent(agent.id).await.unwrap();
    assert!(
        cursor
            .add_events(agent.id, &[state_change(AgentState::Idle)])
            .await
            .is_err()
    );
    let after = cursor.get_agent(agent.id).await.unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.next_event_id, before.next_event_id);
    assert_eq!(after.updated_at, before.updated_at);
    cursor.commit().await.unwrap();
    db.close().await;

    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );
    let events = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    let context = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
    assert_eq!(context.status, AgentState::Running);
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn forks_inherit_state_at_the_selected_event_boundary() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let empty = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "empty".into(),
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(
            agent.id,
            &[
                log("before state changes"),
                state_change(AgentState::Running),
                state_change(AgentState::Interrupted),
            ],
        )
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: EventId(2),
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(agent.id))
        .unwrap()
        .clone();
    let empty_child = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(empty.id))
        .unwrap();
    assert_eq!(child.state, AgentState::Running);
    assert_eq!(empty_child.state, AgentState::Idle);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    cursor
        .add_events(child.id, &[state_change(AgentState::Idle)])
        .await
        .unwrap();

    let nested = cursor
        .fork_session(
            fork.session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: child.id,
                    event_id: EventId(1),
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(
        nested
            .agents
            .iter()
            .all(|agent| agent.state == AgentState::Idle)
    );
    let head = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let head_child = head
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(agent.id))
        .unwrap();
    assert_eq!(head_child.state, AgentState::Interrupted);
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().state,
        AgentState::Idle
    );
}

#[tokio::test]
async fn concurrent_state_changes_match_the_committed_event_order() {
    let (_dir, db, _session, agent) = fixture().await;
    let append = |state| {
        let db = db.clone();
        async move {
            let mut cursor = db.cursor().await.unwrap();
            let saved = cursor
                .add_events(agent.id, &[state_change(state)])
                .await
                .unwrap();
            cursor.commit().await.unwrap();
            saved
        }
    };
    tokio::join!(
        append(AgentState::Running),
        append(AgentState::Interrupted),
        append(AgentState::Idle)
    );

    let mut cursor = db.cursor().await.unwrap();
    let events = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    let EventType::StatusChange(last) = &events.last().unwrap().kind else {
        panic!()
    };
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        last.new_status
    );
}

#[tokio::test]
async fn whole_session_forks_freeze_all_streams_and_support_older_inherited_forks() {
    let (_dir, db, session, a) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let b = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "worker".into(),
            },
        )
        .await
        .unwrap();
    let empty = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "empty".into(),
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(a.id, &[log("a1"), log("a2")])
        .await
        .unwrap();
    cursor.add_events(b.id, &[log("b1")]).await.unwrap();
    cursor.add_events(a.id, &[log("a3")]).await.unwrap();
    cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "too late".into(),
            },
        )
        .await
        .unwrap();
    cursor.add_events(b.id, &[log("b2")]).await.unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: b.id,
                    event_id: EventId(1),
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.agents.len(), 3);
    let child_a = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(a.id))
        .unwrap()
        .clone();
    let child_b = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(b.id))
        .unwrap()
        .clone();
    let child_empty = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(empty.id))
        .unwrap()
        .clone();
    assert_eq!(
        cursor
            .load_events(child_a.id, EventRange::default())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        cursor
            .load_events(child_b.id, EventRange::default())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        cursor
            .load_events(child_empty.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    cursor.add_events(a.id, &[log("a4")]).await.unwrap();
    assert_eq!(
        cursor
            .add_events(child_b.id, &[log("child b2")])
            .await
            .unwrap()
            .events[0]
            .id,
        EventId(2)
    );
    let nested = cursor
        .fork_session(
            fork.session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: child_a.id,
                    event_id: EventId(1),
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let nested_a = nested
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(child_a.id))
        .unwrap();
    let nested_b = nested
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(child_b.id))
        .unwrap();
    let page = cursor
        .load_events(nested_a.id, EventRange::default())
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].source_agent_id, a.id);
    assert!(
        cursor
            .load_events(nested_b.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    let local = cursor
        .add_events(nested_a.id, &[log("nested a2")])
        .await
        .unwrap();
    assert_eq!(local.events[0].id, EventId(2));
    let page = cursor
        .load_events(
            nested_a.id,
            EventRange {
                after: EventId(1),
                through: Some(EventId(2)),
                limit: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(page[0].source_agent_id, nested_a.id);
    assert_eq!(
        cursor.list_sessions(Some(session.id)).await.unwrap().len(),
        1
    );
    assert!(
        cursor
            .fork_session(
                session.id,
                SessionFork {
                    point: ForkPoint::ThroughEvent {
                        agent_id: child_a.id,
                        event_id: EventId(1)
                    },
                    metadata: Default::default()
                }
            )
            .await
            .is_err()
    );
    let blank = cursor
        .create_session(NewSession {
            model_config: crate::test_support::model_config(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "blank".into(),
            working_directory: "/".into(),
        })
        .await
        .unwrap();
    assert!(
        cursor
            .fork_session(
                blank.id,
                SessionFork {
                    point: ForkPoint::Head,
                    metadata: Default::default()
                }
            )
            .await
            .unwrap()
            .agents
            .is_empty()
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn concurrent_writers_reserve_unique_positions_and_sequences() {
    let (_dir, db, session, a) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let b = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "other".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let append = |agent| {
        let db = db.clone();
        async move {
            for _ in 0..8 {
                let mut cursor = db.cursor().await.unwrap();
                cursor
                    .add_events(agent, &[log("batch"), log("batch")])
                    .await
                    .unwrap();
                cursor.commit().await.unwrap();
            }
        }
    };
    tokio::join!(append(a.id), append(a.id), append(b.id));
    let mut cursor = db.cursor().await.unwrap();
    let a_events = cursor
        .load_events(a.id, EventRange::default())
        .await
        .unwrap();
    assert_eq!(a_events.len(), 32);
    for (i, event) in a_events.iter().enumerate() {
        assert_eq!(event.id.0, i as u64 + 1);
    }
    let (count, unique): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(DISTINCT session_position) FROM events")
            .fetch_one(cursor.connection())
            .await
            .unwrap();
    assert_eq!((count, unique), (48, 48));
    assert_eq!(
        cursor.get_session(session.id).await.unwrap().next_position,
        51
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn missing_payload_and_invalid_numbers_are_reported() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor.add_events(agent.id, &[log("test")]).await.unwrap();
    sqlx::query("DELETE FROM event_log")
        .execute(cursor.connection())
        .await
        .unwrap();
    assert!(matches!(
        cursor.load_events(agent.id, EventRange::default()).await,
        Err(DbError::Corrupt(_))
    ));
    let huge = NewEvent::new(EventType::Compaction(Compaction {
        summary: CompactionSummary::Text("x".into()),
        usage: Usage {
            input_tokens: Some(u64::MAX),
            ..Usage::default()
        },
    }));
    assert!(matches!(
        cursor.add_events(agent.id, &[huge]).await,
        Err(DbError::InvalidInput(_))
    ));
    assert!(
        cursor
            .load_events(
                agent.id,
                EventRange {
                    limit: 0,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn sqlite_enforces_payload_kind_and_failed_forks_leave_no_children() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor.add_events(agent.id, &[log("test")]).await.unwrap();
    let pk: i64 = sqlx::query_scalar("SELECT event_pk FROM events LIMIT 1")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO event_new_context (event_pk) VALUES (?)")
            .bind(pk)
            .execute(cursor.connection())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE events SET kind = 'unknown'")
            .execute(cursor.connection())
            .await
            .is_err()
    );
    // Simulate a corrupt ancestry after a crash/manual edit. Fork creates its
    // session before walking streams; even that row must roll back on error.
    sqlx::query("UPDATE agents SET parent_agent_id = id WHERE id = ?")
        .bind(agent.id.0 as i64)
        .execute(cursor.connection())
        .await
        .unwrap();
    assert!(matches!(
        cursor
            .fork_session(
                session.id,
                SessionFork {
                    point: ForkPoint::Head,
                    metadata: Default::default()
                }
            )
            .await,
        Err(DbError::Corrupt(_))
    ));
    assert!(
        cursor
            .list_sessions(Some(session.id))
            .await
            .unwrap()
            .is_empty()
    );
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    cursor.commit().await.unwrap();
    db.close().await;
}

async fn create_legacy_database(path: &std::path::Path) -> sqlx::SqlitePool {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let initial = sqlx::migrate::Migration::new(
        1,
        "events".into(),
        sqlx::migrate::MigrationType::Simple,
        include_str!("../../migrations/0001_events.sql").into_sql_str(),
        false,
    );
    sqlx::migrate::Migrator::with_migrations(vec![initial])
        .run(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agent_definitions (id,version,name,description,selectable,tools,agent_create_hooks,session_update_hooks,before_request_hooks,after_request_hooks,skills,permitted_subagents,extra_system_prompt) VALUES (1,1,'test','',1,'[]','[]','[]','[]','[]','[]','[]','')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!("INSERT INTO probes VALUES ('00000000-0000-0000-0000-000000000000','{}',0,0)")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn backfills_agent_states_from_bounded_legacy_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    sqlx::raw_sql(
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,5,'root','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (2,1,4,5,'child','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (3,2,4,4,'grandchild','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (4,1,4,4,'sibling','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,3,1,'root',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (2,2,1,2,3,1,'child',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (3,3,2,2,2,1,'grandchild',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (4,1,NULL,1,1,2,'empty',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (5,4,1,2,2,1,'sibling',0,0,1);
         INSERT INTO events VALUES (1,1,1,3,0,'status_change');
         INSERT INTO events VALUES (2,1,2,4,0,'status_change');
         INSERT INTO events VALUES (3,2,2,4,0,'status_change');
         INSERT INTO event_status_change (event_pk,status) VALUES (1,'running'),(2,'interrupted'),(3,'idle');",
    ).execute(&pool).await.unwrap();
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    for (id, expected) in [
        (1, AgentState::Interrupted),
        (2, AgentState::Idle),
        (3, AgentState::Running),
        (4, AgentState::Idle),
        (5, AgentState::Running),
    ] {
        let id = crate::agents::AgentId(id);
        assert_eq!(cursor.get_agent(id).await.unwrap().state, expected);
        let events = cursor.load_events(id, EventRange::default()).await.unwrap();
        let context = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
        assert_eq!(context.status, expected);
    }
    assert!(
        sqlx::query("UPDATE agents SET state = 'invalid' WHERE id = 1")
            .execute(cursor.connection())
            .await
            .is_err()
    );
    cursor.commit().await.unwrap();
    db.close().await;
    let db = Database::open(&path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_agent(crate::agents::AgentId(3))
            .await
            .unwrap()
            .state,
        AgentState::Running
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn upgrades_legacy_delimiters_and_backfills_inherited_model_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;

    // Parent appends a newer model after the fork. The child must inherit the
    // model visible at its fork, not the parent's later configuration.
    sqlx::raw_sql(
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,4,'parent','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (2,1,3,4,'child','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,3,1,'parent agent',0,0,1);
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (2,2,1,2,3,1,'child agent',0,0,1);
         INSERT INTO events VALUES (1,1,1,2,0,'model');
         INSERT INTO event_model (event_pk,provider,model,effort) VALUES (1,'openrouter','old-model','high');
         INSERT INTO events VALUES (2,1,2,3,0,'model');
         INSERT INTO event_model (event_pk,provider,model,effort) VALUES (2,'openrouter','later-model','low');
         INSERT INTO events VALUES (3,2,2,3,0,'inference_start');
         INSERT INTO event_inference_start (event_pk,inference_id,max_output_tokens) VALUES (3,17,42);"
    ).execute(&pool).await.unwrap();
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let events = cursor
        .load_events(crate::agents::AgentId(2), EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    let EventType::InferenceResultStart(start) = &events[1].kind else {
        panic!()
    };
    assert_eq!(start.id, 17);
    assert_eq!(start.model_config.model.api_name, "old-model");
    assert_eq!(start.model_config.effort, Effort::High);
    assert_eq!(start.request_config.max_output_tokens, Some(42));
    assert_eq!(events[1].id, EventId(2));
    assert_eq!(events[1].source_agent_id, crate::agents::AgentId(2));
    let state = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
    assert_eq!(state.open_inference_result().unwrap().id, 17);
    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(violations, 0);
    cursor.commit().await.unwrap();
    db.close().await;
    Database::open(&path).await.unwrap().close().await;
}

#[tokio::test]
async fn upgrades_legacy_tool_results_to_content_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    sqlx::raw_sql(
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,5,'test','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,4,1,'main',0,0,1);
         INSERT INTO events VALUES (1,1,1,2,0,'tool_result');
         INSERT INTO events VALUES (2,1,2,3,0,'tool_result');
         INSERT INTO events VALUES (3,1,3,4,0,'tool_result');
         INSERT INTO event_tool_result (event_pk,call_id,contents,error,metadata,replay_content)
         VALUES (1,'plain','plain text',1,'{\"item_id\":\"legacy-result\",\"provider_metadata\":{}}',NULL),
                (2,'rich','display text',0,'{\"provider_metadata\":{}}','[{\"Text\":{\"text\":\"actual text\",\"annotations\":[]}},{\"Image\":{\"source\":{\"Url\":\"https://example.test/output.png\"},\"detail\":null}}]'),
                (3,'empty','not sent',0,'{\"provider_metadata\":{}}','[]');",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let events = cursor
        .load_events(crate::agents::AgentId(1), EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    let mut results = vec![];
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.id, EventId(index as u64 + 1));
        let EventType::ToolResult(result) = &event.kind else {
            panic!()
        };
        results.push(result);
    }
    assert!(results[0].error);
    assert_eq!(results[0].call_id, "plain");
    assert_eq!(
        results[0].metadata.item_id.as_deref(),
        Some("legacy-result")
    );
    assert_eq!(results[0].content.len(), 1);
    assert!(
        matches!(&results[0].content[0], api::ContentBlock::Text(text) if text.text == "plain text")
    );
    assert!(!results[1].error);
    assert_eq!(results[1].call_id, "rich");
    assert_eq!(results[1].content.len(), 2);
    assert!(
        matches!(&results[1].content[0], api::ContentBlock::Text(text) if text.text == "actual text")
    );
    assert!(matches!(
        &results[1].content[1],
        api::ContentBlock::Image(_)
    ));
    assert_eq!(results[2].call_id, "empty");
    assert!(results[2].content.is_empty());

    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(violations, 0);
    cursor.commit().await.unwrap();
    db.close().await;
    Database::open(&path).await.unwrap().close().await;
}

#[tokio::test]
async fn upgrades_legacy_messages_to_content_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    sqlx::raw_sql(
        "INSERT INTO sessions (id,parent_session_id,first_position,next_position,name,working_directory,probe_id,created_at_us,updated_at_us,model_config) VALUES (1,NULL,1,5,'test','/work','00000000-0000-0000-0000-000000000000',0,0,'{\"model\":{\"provider\":\"Openrouter\",\"api_name\":\"test-model\"},\"effort\":\"High\"}');
         INSERT INTO agents (id,session_id,parent_agent_id,first_event_seq,next_event_seq,created_position,name,created_at_us,updated_at_us,definition_id) VALUES (1,1,NULL,1,4,1,'main',0,0,1);
         INSERT INTO events VALUES (1,1,1,2,0,'message');
         INSERT INTO events VALUES (2,1,2,3,0,'message');
         INSERT INTO events VALUES (3,1,3,4,0,'message');
         INSERT INTO event_message (event_pk,message_type,delivery,contents,metadata,replay_content)
         VALUES (1,'human','queue','plain text','{\"item_id\":\"legacy-message\",\"provider_metadata\":{}}',NULL),
                (2,'agent',NULL,'display text','{\"provider_metadata\":{}}','[{\"Text\":{\"text\":\"actual text\",\"annotations\":[]}},{\"Image\":{\"source\":{\"Url\":\"https://example.test/image.png\"},\"detail\":null}}]'),
                (3,'harness','steer','not sent','{\"provider_metadata\":{}}','[]');",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let events = cursor
        .load_events(crate::agents::AgentId(1), EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    let mut messages = vec![];
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.id, EventId(index as u64 + 1));
        let EventType::Message(message) = &event.kind else {
            panic!()
        };
        messages.push(message);
    }
    assert_eq!(messages[0].message_type, MessageType::Human);
    assert_eq!(
        messages[0].metadata.item_id.as_deref(),
        Some("legacy-message")
    );
    assert_eq!(messages[0].content.len(), 1);
    assert!(
        matches!(&messages[0].content[0], api::ContentBlock::Text(text) if text.text == "plain text")
    );
    assert_eq!(messages[1].message_type, MessageType::Agent);
    assert_eq!(messages[1].content.len(), 2);
    assert!(
        matches!(&messages[1].content[0], api::ContentBlock::Text(text) if text.text == "actual text")
    );
    assert!(matches!(
        &messages[1].content[1],
        api::ContentBlock::Image(_)
    ));
    assert_eq!(messages[2].message_type, MessageType::Harness);
    assert!(messages[2].content.is_empty());

    let context = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
    assert_eq!(context.items().len(), 3);
    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(violations, 0);
    cursor.commit().await.unwrap();
    db.close().await;
    Database::open(&path).await.unwrap().close().await;
}

#[tokio::test]
async fn atomically_persists_and_replays_a_complete_result_block() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let saved = cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::Model(model())),
                NewEvent::new(EventType::Message(Message::text(
                    MessageType::Human,
                    "question",
                ))),
                NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                    id: 1,
                    model_config: model(),
                    request_config: api::RequestConfig::default(),
                })),
                NewEvent::new(EventType::Message(Message::text(
                    MessageType::Agent,
                    "answer",
                ))),
                NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                    id: 1,
                    outcome: Outcome::Completed {
                        reason: StopReason::EndTurn,
                    },
                    usage: usage(),
                    request_time: Duration::from_micros(10),
                    response_id: Some("response".into()),
                    provider_metadata: Default::default(),
                })),
            ],
        )
        .await
        .unwrap();
    let prefix = cursor
        .load_events(
            agent.id,
            EventRange {
                through: Some(saved.events[3].id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let incomplete = crate::agents::agent_projection::AgentProjection::replay(&prefix).unwrap();
    assert!(matches!(
        incomplete.prepare_request(api::RequestConfig::default()),
        Err(crate::agents::agent_projection::RequestError::OpenInferenceResult(1))
    ));
    let loaded = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    let state = crate::agents::agent_projection::AgentProjection::replay(&loaded).unwrap();
    assert_eq!(state.items().len(), 2);
    assert_eq!(
        state.last_response().unwrap().response_id.as_deref(),
        Some("response")
    );
    assert_eq!(state.last_response().unwrap().usage.input_tokens, Some(0));
    let request = state
        .prepare_request(api::RequestConfig::default())
        .unwrap();
    assert_eq!(request.history.items.len(), 2);
    cursor.commit().await.unwrap();
    db.close().await;
}
