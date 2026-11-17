use super::*;
use crate::components::bundles::codemode::MontyEngine;
use crate::components::{
    codemode_functions::CodeModeFunctionKind,
    tools::{CodeMode, Noop},
};
use crate::db::NewEvent;
use crate::db::PrivateEvent;
use serde_json::json;

#[tokio::test]
async fn nested_contexts_have_independent_snapshots_and_shared_registry() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let mut components = crate::components::bundles::InstalledComponents::default();
    components.codemode_functions =
        crate::components::codemode_functions::listed_entries([CodeModeFunctionKind::Tool(
            ToolKind::Noop(Noop),
        )]);
    let registry = components.codemode_functions.clone();
    state
        .attach_components(Arc::new(tokio::sync::Mutex::new(components)))
        .await;
    assert!(Arc::ptr_eq(&registry, &state.codemode_functions));
    let snapshot = state.invocation_snapshot();
    assert!(Arc::ptr_eq(
        &snapshot.codemode_functions,
        &state.codemode_functions
    ));
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let parent = AgentContext::invocation(snapshot, &probe, &fixture.runtime.handle);
    let mut first = parent.child_invocation().unwrap();
    let second = parent.child_invocation().unwrap();
    assert_eq!(parent.codemode_functions().len(), 1);
    assert_eq!(first.codemode_functions().len(), 1);
    let old_id = parent.projection().last_event_id;
    let events = [NewEvent::new(EventType::Log(Log {
        error: false,
        message: "child save".into(),
    }))];
    let save = first.save_events(&events);
    let service = async {
        let request = state.invocation_saves.recv().await.unwrap();
        let events = state.persist_event_batch(request.events).await.unwrap();
        let _ = request.reply.send(Ok(state.saved_events(events)));
    };
    let (saved, ()) = tokio::join!(save, service);
    let saved = saved.unwrap();
    assert_eq!(first.projection().last_event_id, saved[0].id);
    assert_eq!(second.projection().last_event_id, old_id);
    assert_eq!(parent.projection().last_event_id, old_id);

    let mixed = [
        EventBatchItem::Public(result_event("nested")),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![1])),
    ];
    let save = first.save_event_batch(&mixed);
    let service = async {
        let request = state.invocation_saves.recv().await.unwrap();
        let events = state.persist_event_batch(request.events).await.unwrap();
        let _ = request.reply.send(Ok(state.saved_events(events)));
    };
    let (saved, ()) = tokio::join!(save, service);
    let saved = saved.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(first.projection().last_event_id, saved[0].id);
    assert_eq!(second.projection().last_event_id, old_id);
    assert_eq!(
        first
            .load_latest_private_event(crate::db::PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(vec![1]))
    );

    drop((first, second));
    drop(parent);
    let mut borrowed = AgentContext::new(&mut state, &probe, &fixture.runtime.handle);
    assert!(borrowed.child_invocation().is_none());
    let mixed = [
        EventBatchItem::Public(result_event("borrowed")),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![2])),
    ];
    let saved = borrowed.save_event_batch(&mixed).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(borrowed.projection().last_event_id, saved[0].id);
    assert_eq!(
        borrowed
            .load_latest_private_event(crate::db::PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(vec![2]))
    );
    drop(borrowed);
    fixture.close().await;
}

fn result_event(id: &str) -> NewEvent {
    NewEvent::new(EventType::ToolResult(ToolResult {
        delivery: None,
        call_id: id.into(),
        content: prompts::text("ok"),
        error: false,
        metadata: Default::default(),
    }))
}

#[tokio::test]
async fn mixed_batch_commits_public_and_private_events() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let mut subscriber = fixture.publication.lock().await.subscribe();
    let previous = state.agent.next_event_id;
    state.start_persistence(
        vec![
            EventBatchItem::Public(result_event("mixed")),
            EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![1, 2, 3])),
        ],
        &[],
    );
    let saved = state.finish_persistence().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(state.agent.next_event_id.0, previous.0 + 1);
    let mut committed = 0;
    while let Ok(message) = subscriber.try_recv() {
        if let psa_api::StreamMessage::Committed { event, .. } = message {
            committed += 1;
            assert!(matches!(event.event.kind, EventType::ToolResult(_)));
        }
    }
    assert_eq!(committed, 1);
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(vec![1, 2, 3]))
    );
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    assert!(
        matches!(&events.last().unwrap().kind, EventType::ToolResult(result) if result.call_id == "mixed")
    );
    cursor.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn dropped_worker_does_not_cancel_inflight_batch() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let agent = state.agent.id;
    let gate = fixture.publication.lock().await;
    state.start_persistence(
        vec![
            EventBatchItem::Public(result_event("dropped-worker")),
            EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![9])),
        ],
        &[],
    );
    drop(state);
    let database = fixture.database.clone();
    let probes = fixture.probes.clone();
    let mut replacement = tokio::spawn(async move {
        Runtime::start(
            database,
            Publication::default(),
            ProviderRegistry::default(),
            probes,
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut replacement)
            .await
            .is_err(),
        "recovery must wait for the blocked commit"
    );
    drop(gate);
    let replacement = tokio::time::timeout(Duration::from_secs(5), replacement)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let recovered =
        AgentManagerState::load_persisted(fixture.database.clone(), Publication::default(), agent)
            .await
            .unwrap();
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(agent, None, crate::db::PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        Some(PrivateEvent::CodeModeSnapshot(vec![9]))
    );
    let events = cursor.load_all_events(agent).await.unwrap();
    assert!(events.iter().any(
        |event| matches!(&event.kind, EventType::ToolResult(result) if result.call_id == "dropped-worker")
    ));
    assert_eq!(recovered.context.last_event_id, events.last().unwrap().id);
    cursor.rollback().await.unwrap();
    replacement.shutdown().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn invalid_private_event_rolls_back_public_result() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let previous = state.agent.next_event_id;
    state.start_persistence(
        vec![
            EventBatchItem::Public(result_event("invalid")),
            EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![
                0;
                crate::db::MAX_CODEMODE_SNAPSHOT_BYTES
                    + 1
            ])),
        ],
        &[],
    );
    assert!(state.finish_persistence().await.is_err());
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_agent(state.agent.id)
            .await
            .unwrap()
            .next_event_id,
        previous
    );
    assert_eq!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap(),
        None
    );
    cursor.rollback().await.unwrap();
    fixture.close().await;
}

#[test]
fn batch_filter_keeps_private_anchors_and_remaps_public_promotions() {
    let batch = vec![
        EventBatchItem::Public(status_event(AgentState::Running)),
        EventBatchItem::Public(result_event("first")),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![1])),
        EventBatchItem::Public(status_event(AgentState::Running)),
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![2])),
        EventBatchItem::Public(result_event("last")),
    ];
    let promotions = vec![
        (0, "skipped".into()),
        (1, "first".into()),
        (2, "anchor".into()),
        (3, "last".into()),
    ];
    let (filtered, promotions) = build_persistence_batch(batch, &promotions, AgentState::Running);
    assert_eq!(filtered.len(), 5);
    assert!(
        matches!(&filtered[0], EventBatchItem::Public(event) if matches!(&event.kind, EventType::ToolResult(result) if result.call_id == "first"))
    );
    assert!(matches!(&filtered[1], EventBatchItem::Private(_)));
    assert!(
        matches!(&filtered[2], EventBatchItem::Public(event) if matches!(&event.kind, EventType::StatusChange(_)))
    );
    assert!(matches!(&filtered[3], EventBatchItem::Private(_)));
    assert_eq!(
        promotions,
        vec![
            (0, "first".into()),
            (1, "anchor".into()),
            (2, "last".into())
        ]
    );
}

#[tokio::test]
async fn leading_private_event_is_rejected_without_publication() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let previous = state.agent.next_event_id;
    let mut subscriber = fixture.publication.lock().await.subscribe();
    let batch = vec![
        EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(vec![7])),
        EventBatchItem::Public(status_event(AgentState::Running)),
        EventBatchItem::Public(result_event("invalid-leading")),
    ];
    assert!(state.persist_event_batch(batch).await.is_err());
    assert_eq!(state.agent.next_event_id, previous);
    assert!(subscriber.try_recv().is_err());
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_agent(state.agent.id)
            .await
            .unwrap()
            .next_event_id,
        previous
    );
    assert_eq!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap(),
        None
    );
    cursor.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn draining_saves_replies_to_invalid_batch_and_persists_next_request() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let agent = state.agent.id;
    let saves = state.invocation_save_handle.clone();
    let invalid = saves.request(|reply| SaveRequest {
        events: vec![EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(
            vec![7],
        ))],
        reply,
    });
    let valid = saves.request(|reply| SaveRequest {
        events: vec![EventBatchItem::Public(result_event("after-invalid"))],
        reply,
    });
    let service = async { state.drain_invocation_saves().await.unwrap() };
    let (invalid, valid, ()) = tokio::join!(invalid, valid, service);
    assert!(matches!(
        invalid.unwrap(),
        Err(AgentError::Database(DbError::InvalidInput(_)))
    ));
    let saved = valid.unwrap().unwrap();
    assert_eq!(saved.events.len(), 1);
    assert_eq!(saved.projection.last_event_id, saved.events[0].id);
    let mut cursor = fixture.database.cursor().await.unwrap();
    let events = cursor.load_all_events(agent).await.unwrap();
    assert!(
        matches!(&events.last().unwrap().kind, EventType::ToolResult(result) if result.call_id == "after-invalid")
    );
    assert_eq!(
        cursor
            .load_latest_private_event(agent, None, crate::db::PrivateEventKind::CodeModeSnapshot)
            .await
            .unwrap(),
        None
    );
    cursor.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn sequential_codemode_calls_observe_committed_private_events() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let state = fixture
        .state(
            vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))],
            &[
                ("first", "codemode", json!({"code": "x = 41"})),
                ("second", "codemode", json!({"code": "print(x + 1)"})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    let mut cursor = fixture.database.cursor().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.rollback().await.unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == "second" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(!result.error, "{:?}", result.content);
    assert!(format!("{:?}", result.content).contains("42"));
    drop(state);
    engine.close().await;
    fixture.close().await;
}
