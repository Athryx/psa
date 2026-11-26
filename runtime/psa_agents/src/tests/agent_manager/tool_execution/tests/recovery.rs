use super::*;
use psa_store::NewEvent;
use std::panic::AssertUnwindSafe;

#[tokio::test]
async fn batch_snapshots_share_the_base_projection_and_saves_replace_only_the_writer() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(Vec::new(), &[]).await;
    let snapshot = state.invocation_snapshot();
    let peer_snapshot = snapshot.clone();
    assert!(Arc::ptr_eq(&snapshot.projection, &peer_snapshot.projection));
    let original = snapshot.projection.last_event_id;
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut writer = AgentContext::invocation(snapshot, &probe, &fixture.runtime.handle);
    let peer = AgentContext::invocation(peer_snapshot, &probe, &fixture.runtime.handle);
    assert!(std::ptr::eq(writer.projection(), peer.projection()));

    let events = [NewEvent::new(EventType::Log(Log {
        error: false,
        message: "writer-only snapshot refresh".into(),
    }))];
    {
        let save = writer.save_events(&events);
        tokio::pin!(save);
        assert!(futures_util::poll!(&mut save).is_pending());
        state.drain_invocation_saves().await.unwrap();
        save.await.unwrap();
    }
    assert_eq!(peer.projection().last_event_id, original);
    assert_eq!(writer.projection().last_event_id.0, original.0 + 1);
    assert!(!std::ptr::eq(writer.projection(), peer.projection()));
    drop(writer);
    drop(peer);
    fixture.close().await;
}

#[tokio::test]
async fn sequential_barriers_include_preceding_immediate_error_results() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (sequential, release) = tool(ToolId::Read, ExecutionMode::Sequential, &sender);
    release.add_permits(1);
    let state = fixture
        .state(
            vec![Box::new(sequential) as Box<dyn crate::tools::DynTool>],
            &[
                ("unknown", "absent", serde_json::json!({})),
                (
                    "sequential",
                    "sequential",
                    serde_json::json!({"label":"sequential"}),
                ),
            ],
        )
        .await;
    let initial = state.agent.next_event_id;
    let (controls, run) = fixture.run(state).await;
    match notice(&mut notices).await {
        Notice::Enter(label, next, ..) => {
            assert_eq!(label, "sequential");
            assert_eq!(next.0, initial.0 + 1);
        }
        other => panic!("unexpected {other:?}"),
    }
    finish(run).await;
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn each_invocation_has_its_own_cancellation_grace_period() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (mut fast, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let (mut slow, release) = tool(ToolId::Edit, ExecutionMode::Parallel, &sender);
    let fast_dropped = {
        let tool = &mut fast;
        tool.ignore_cancellation = true;
        tool.grace = Duration::ZERO;
        tool.dropped.clone()
    };
    let slow_dropped = {
        let tool = &mut slow;
        tool.ignore_cancellation = true;
        tool.grace = Duration::from_secs(4);
        tool.dropped.clone()
    };
    let state = fixture
        .state(
            vec![
                Box::new(fast) as Box<dyn crate::tools::DynTool>,
                Box::new(slow) as Box<dyn crate::tools::DynTool>,
            ],
            &[
                ("fast", "parallel", serde_json::json!({"label":"fast"})),
                ("slow", "later", serde_json::json!({"label":"slow"})),
            ],
        )
        .await;
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    controls.send(AgentManagerMessage::Interrupt).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fast_dropped.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(slow_dropped.load(Ordering::SeqCst), 0);
    release.add_permits(1);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Interrupted);
    assert_eq!(slow_dropped.load(Ordering::SeqCst), 1);
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn enqueued_saves_survive_a_sibling_panic_before_mailbox_handling() {
    let fixture = Fixture::new().await;
    let (sender, _notices) = mpsc::unbounded_channel();
    let (parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let dropped = {
        let tool = &parallel;
        tool.dropped.clone()
    };
    let mut state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                (
                    "save",
                    "parallel",
                    serde_json::json!({"label":"save", "saves":1}),
                ),
                (
                    "panic",
                    "parallel",
                    serde_json::json!({"label":"panic", "panic":true}),
                ),
            ],
        )
        .await;
    let (_controls, mut received) = actors::unbounded_channel();
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let panic =
        AssertUnwindSafe(state.resolve_tools(&probe, &fixture.runtime.handle, &mut received))
            .catch_unwind()
            .await;
    assert!(panic.is_err());
    // Unwinding drops ALL sibling invocations before persistence recovery.
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    state.finish_persistence().await.unwrap();
    state.drain_invocation_saves().await.unwrap();
    let events = build_recovery_events(&state.agent, &state.context).unwrap();
    state.persist_events(&events).await.unwrap();
    assert!(state.outstanding_calls().is_empty());
    assert_eq!(state.agent.state, AgentState::Interrupted);
    let mut cursor = fixture.database.read().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.close().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::Log(log) if log.message == "save-0"))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::ToolResult(_)))
            .count(),
        2
    );
    fixture.close().await;
}

#[tokio::test]
async fn timed_out_save_waiter_does_not_cancel_its_enqueued_commit() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (mut parallel, _) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    let dropped = {
        let tool = &mut parallel;
        tool.grace = Duration::ZERO;
        tool.dropped.clone()
    };
    let state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[
                (
                    "save",
                    "parallel",
                    serde_json::json!({"label":"save", "saves":1}),
                ),
                ("wait", "parallel", serde_json::json!({"label":"wait"})),
            ],
        )
        .await;
    let publication = fixture.publication.lock().await;
    let (controls, run) = fixture.run(state).await;
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    assert!(matches!(notice(&mut notices).await, Notice::Enter(..)));
    controls.send(AgentManagerMessage::Interrupt).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while dropped.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Controls and individual grace timeouts were serviced despite a blocked
    // publication append. Only the owned commit is still holding up the worker.
    assert!(!run.is_finished());
    drop(publication);
    let state = finish(run).await;
    assert_eq!(state.agent.state, AgentState::Interrupted);
    let mut cursor = fixture.database.read().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.close().await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::Log(log) if log.message == "save-0"))
            .count(),
        1
    );
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == "save" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(result.error);
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::to_value(prompts::tool_timed_out()).unwrap()
    );
    drop(controls);
    fixture.close().await;
}

#[tokio::test]
async fn malformed_unknown_incomplete_and_typed_invalid_calls_each_get_one_result() {
    let fixture = Fixture::new().await;
    let (sender, mut notices) = mpsc::unbounded_channel();
    let (parallel, release) = tool(ToolId::ListSessions, ExecutionMode::Parallel, &sender);
    release.add_permits(1);
    let mut state = fixture
        .state(
            vec![Box::new(parallel) as Box<dyn crate::tools::DynTool>],
            &[("valid", "parallel", serde_json::json!({"label":"valid"}))],
        )
        .await;
    let mut events = Vec::new();
    for (call_id, name, arguments, incomplete) in [
        ("malformed", "parallel", "{", false),
        ("unknown", "absent", "{}", false),
        ("incomplete", "parallel", "{}", true),
        ("invalid", "parallel", "{}", false),
    ] {
        let mut metadata = llm_api::api::HistoryItemMetadata::default();
        if incomplete {
            metadata.item_status = Some(llm_api::api::ItemStatus::Incomplete);
        }
        events.push(NewEvent::new(EventType::ToolCall(ToolCall {
            call_id: call_id.into(),
            tool_name: name.into(),
            arguments: arguments.into(),
            metadata,
        })));
    }
    state.persist_events(&events).await.unwrap();
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    let results: Vec<_> = state
        .context
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 5);
    assert_eq!(results.iter().filter(|result| result.is_error).count(), 4);
    assert!(matches!(notice(&mut notices).await, Notice::Enter(label, ..) if label == "valid"));
    assert!(matches!(notice(&mut notices).await, Notice::Exit(label) if label == "valid"));
    assert!(notices.try_recv().is_err());
    drop(controls);
    fixture.close().await;
}
