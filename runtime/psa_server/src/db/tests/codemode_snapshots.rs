use super::*;
use crate::db::{EventRange, NewEvent};
use crate::{
    agents::AgentId,
    db::{
        DatabaseCursor, EventBatchItem, MAX_CODEMODE_SNAPSHOT_BYTES, PrivateEvent, PrivateEventKind,
    },
};

// Exercise the generic batch API while keeping lineage scenarios concise.
impl DatabaseCursor {
    async fn add_snapshot(
        &mut self,
        agent: AgentId,
        event: &NewEvent,
        snapshot: &[u8],
    ) -> Result<crate::db::AddEventResult, crate::db::DbError> {
        self.add_event_batch(
            agent,
            &[
                EventBatchItem::Public(event.clone()),
                EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(snapshot.to_vec())),
            ],
        )
        .await
    }
}

fn tool_result(id: &str) -> NewEvent {
    NewEvent::new(EventType::ToolResult(ToolResult {
        delivery: None,
        call_id: id.into(),
        content: vec![],
        error: false,
        metadata: Default::default(),
    }))
}

#[tokio::test]
async fn snapshots_follow_fork_lineage_and_never_enter_public_events() {
    let (dir, db, session, parent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let first = cursor
        .add_snapshot(parent.id, &tool_result("one"), b"private-first\0")
        .await
        .unwrap()
        .events[0]
        .id;
    cursor
        .add_events(parent.id, &[log("ordinary")])
        .await
        .unwrap();
    let second = cursor
        .add_snapshot(parent.id, &tool_result("two"), b"private-second")
        .await
        .unwrap()
        .events[0]
        .id;
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: parent.id,
                    event_id: first,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = fork.agents[0].id;
    let unrelated = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa::api::AgentDefinitionId(1),
                name: "unrelated".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(unrelated.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        cursor
            .load_latest_private_event(parent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-second".to_vec()))
    );
    assert_eq!(
        cursor
            .load_latest_private_event(parent.id, Some(first), PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-first\0".to_vec()))
    );
    assert_eq!(
        cursor
            .load_latest_private_event(child, Some(EventId(0)), PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        cursor
            .load_latest_private_event(child, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-first\0".to_vec()))
    );
    let child_event = cursor
        .add_snapshot(child, &tool_result("child"), b"private-child")
        .await
        .unwrap()
        .events[0]
        .id;
    assert_eq!(child_event, EventId(first.0 + 1));
    let nested = cursor
        .fork_session(
            fork.session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: child,
                    event_id: child_event,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let grandchild = nested.agents[0].id;
    cursor
        .add_snapshot(child, &tool_result("later child"), b"private-later")
        .await
        .unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(grandchild, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-child".to_vec()))
    );
    assert_eq!(
        cursor
            .load_latest_private_event(grandchild, Some(first), PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-first\0".to_vec()))
    );
    let events = cursor
        .load_events(grandchild, EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![first, child_event]
    );
    assert!(!serde_json::to_string(&events).unwrap().contains("private-"));
    assert_ne!(second, first);
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.cursor().await.unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(grandchild, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-child".to_vec()))
    );
    assert_eq!(
        cursor
            .load_latest_private_event(parent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"private-second".to_vec()))
    );
    cursor.commit().await.unwrap();
}

#[tokio::test]
async fn snapshot_failures_and_cursor_rollback_leave_no_event_or_sequence_gap() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    assert!(
        cursor
            .add_snapshot(agent.id, &log("wrong kind"), b"x")
            .await
            .is_err()
    );
    assert!(
        cursor
            .add_snapshot(
                agent.id,
                &tool_result("oversized"),
                &vec![0; MAX_CODEMODE_SNAPSHOT_BYTES + 1]
            )
            .await
            .is_err()
    );
    sqlx::query("CREATE TEMP TRIGGER reject_snapshot BEFORE INSERT ON codemode_snapshots BEGIN SELECT RAISE(ABORT, 'test failure'); END")
        .execute(cursor.connection()).await.unwrap();
    assert!(
        cursor
            .add_snapshot(agent.id, &tool_result("failed"), b"x")
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER reject_snapshot")
        .execute(cursor.connection())
        .await
        .unwrap();
    assert!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cursor
            .load_latest_private_event(agent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    let saved = cursor
        .add_snapshot(agent.id, &tool_result("rolled back"), b"state")
        .await
        .unwrap();
    assert_eq!(saved.events[0].id, EventId(1));
    cursor.rollback().await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cursor
            .load_latest_private_event(agent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    let saved = cursor
        .add_events(agent.id, &[tool_result("ordinary")])
        .await
        .unwrap();
    assert_eq!(saved.events[0].id, EventId(1));
    assert_eq!(
        cursor
            .load_latest_private_event(agent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    cursor.commit().await.unwrap();
}

#[tokio::test]
async fn mixed_batches_validate_and_commit_public_events_without_gaps() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let private =
        |bytes: &[u8]| EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(bytes.to_vec()));
    let public = |id| EventBatchItem::Public(tool_result(id));

    assert!(
        cursor
            .add_event_batch(agent.id, &[])
            .await
            .unwrap()
            .events
            .is_empty()
    );
    assert!(cursor.add_event_batch(AgentId(999), &[]).await.is_err());
    for invalid in [
        vec![private(b"orphan")],
        vec![public("a"), private(b"first"), private(b"duplicate")],
        vec![
            public("a"),
            EventBatchItem::Public(log("not a result")),
            private(b"wrong"),
        ],
    ] {
        assert!(cursor.add_event_batch(agent.id, &invalid).await.is_err());
    }
    assert!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );

    let saved = cursor
        .add_event_batch(
            agent.id,
            &[
                public("a"),
                private(b"first"),
                EventBatchItem::Public(log("middle")),
                public("b"),
                private(b"second"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        saved.events.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![EventId(1), EventId(2), EventId(3)]
    );
    assert_eq!(
        cursor
            .load_latest_private_event(
                agent.id,
                Some(EventId(2)),
                PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"first".to_vec()))
    );
    assert_eq!(
        cursor
            .load_latest_private_event(agent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(b"second".to_vec()))
    );
    let events = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    assert!(!serde_json::to_string(&events).unwrap().contains("first"));
    cursor.commit().await.unwrap();
}

#[tokio::test]
async fn mixed_batch_failure_rolls_back_all_preceding_items() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    sqlx::query("CREATE TEMP TRIGGER reject_second_snapshot BEFORE INSERT ON codemode_snapshots WHEN (SELECT count(*) FROM codemode_snapshots) >= 1 BEGIN SELECT RAISE(ABORT, 'test failure'); END")
        .execute(cursor.connection()).await.unwrap();
    let batch = [
        EventBatchItem::Public(tool_result("first")),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(b"ok".to_vec())),
        EventBatchItem::Public(tool_result("second")),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(b"bad".to_vec())),
    ];
    assert!(cursor.add_event_batch(agent.id, &batch).await.is_err());
    assert!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cursor
            .load_latest_private_event(agent.id, None, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    let saved = cursor
        .add_events(agent.id, &[log("after failure")])
        .await
        .unwrap();
    assert_eq!(saved.events[0].id, EventId(1));
    cursor.commit().await.unwrap();
}

#[tokio::test]
async fn snapshots_are_compressed_at_rest() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let mut snapshot = b"MONTY\0".to_vec();
    snapshot.extend(std::iter::repeat_n(b'y', 1024 * 1024));
    let event = cursor
        .add_snapshot(agent.id, &tool_result("big"), &snapshot)
        .await
        .unwrap()
        .events[0]
        .id;

    let stored_len: i64 = sqlx::query_scalar("SELECT length(snapshot) FROM codemode_snapshots")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert!(stored_len < 4096, "stored {stored_len} bytes");
    assert_eq!(
        cursor
            .load_private_event_at(agent.id, event, PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(snapshot))
    );
    cursor.commit().await.unwrap();
}

#[test]
fn private_event_debug_redacts_contents() {
    let secret = b"secret snapshot contents";
    let event = PrivateEvent::CodeModeSnapshot(secret.to_vec());
    let private_debug = format!("{event:?}");
    let pending_debug = format!("{:?}", EventBatchItem::Private(event));
    for debug in [private_debug, pending_debug] {
        assert!(debug.contains("CodeModeSnapshot"));
        assert!(debug.contains(&secret.len().to_string()));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(&format!("{:?}", secret.to_vec())));
    }
}
