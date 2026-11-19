//! Native CodeMode reset is a persisted state barrier, not history deletion.

use super::*;
use crate::db::NewEvent;
use crate::{
    components::{
        bundles::codemode::MontyEngine,
        codemode_functions::{CodeModeFunctionKind, prior_results::PriorResults},
        tools::{CodeMode, ResetCodeMode},
    },
    db::{PrivateEvent, PrivateEventKind},
};
use llm_api::api::ContentBlock;
use serde_json::{Value, json};

const RESET_MESSAGE: &str =
    "Python REPL reset. Earlier codemode outputs remain available through prior_results.";

fn tools(engine: &Arc<MontyEngine>) -> Vec<ToolKind> {
    vec![
        ToolKind::CodeMode(CodeMode::new(engine.clone())),
        ToolKind::ResetCodeMode(ResetCodeMode::new(engine.clone())),
    ]
}

fn install_functions(state: &mut AgentManagerState) {
    state.codemode_functions = crate::components::codemode_functions::listed_entries([
        CodeModeFunctionKind::PriorResults(PriorResults),
    ]);
}

async fn append_call(state: &mut AgentManagerState, id: &str, name: &str, arguments: Value) {
    let inference_id = state.agent.next_event_id.0;
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

async fn invoke(
    fixture: &Fixture,
    mut state: AgentManagerState,
    id: &str,
    name: &str,
    arguments: Value,
) -> AgentManagerState {
    append_call(&mut state, id, name, arguments).await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    state
}

async fn load_events(fixture: &Fixture, state: &AgentManagerState) -> Vec<Event> {
    let mut cursor = fixture.database.read().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.close().await.unwrap();
    events
}

async fn tool_result(
    fixture: &Fixture,
    state: &AgentManagerState,
    id: &str,
) -> (EventId, bool, String) {
    let events = load_events(fixture, state).await;
    let (event_id, result) = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == id => Some((event.id, result)),
            _ => None,
        })
        .expect("tool result persisted");
    let text = result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("unexpected tool content: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (event_id, result.error, text)
}

async fn load_snapshot(
    fixture: &Fixture,
    state: &AgentManagerState,
    through: Option<EventId>,
) -> Vec<u8> {
    let mut cursor = fixture.database.read().await.unwrap();
    let event = cursor
        .load_latest_private_event(state.agent.id, through, PrivateEventKind::CodeModeSnapshot)
        .await
        .unwrap()
        .expect("private checkpoint persisted");
    cursor.close().await.unwrap();
    let PrivateEvent::CodeModeSnapshot(bytes) = event else {
        panic!("expected a codemode snapshot");
    };
    bytes
}

async fn inject_snapshot(state: &mut AgentManagerState, id: &str, bytes: &[u8]) -> EventId {
    append_call(state, id, "codemode", json!({"code": "pass"})).await;
    state
        .persist_event_batch(vec![
            EventBatchItem::Public(NewEvent::new(EventType::ToolResult(ToolResult {
                delivery: None,
                call_id: id.into(),
                content: prompts::text("synthetic checkpoint anchor"),
                error: false,
                metadata: Default::default(),
            }))),
            EventBatchItem::Private(PrivateEvent::CodeModeSnapshot(bytes.to_vec())),
        ])
        .await
        .unwrap()
        .last()
        .unwrap()
        .id
}

#[tokio::test]
async fn reset_validation_precancellation_and_missing_worker_preserve_active_checkpoint() {
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(
        fixture
            ._directory
            .path()
            .join("never-created")
            .join("monty-worker"),
    ));
    let reset = ResetCodeMode::new(engine.clone());
    assert_eq!(reset.execution_mode(), ExecutionMode::Sequential);
    let definition = reset.tool_definition();
    assert_eq!(definition.name, "reset_codemode");
    let schema = definition.parameters.as_value();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert!(
        schema
            .get("properties")
            .is_none_or(|properties| properties == &json!({}))
    );

    let mut state = fixture.state(vec![], &[]).await;
    let original = b"previous checkpoint bytes must survive every failed reset";
    let original_id = inject_snapshot(&mut state, "seed", original).await;
    let before_events = load_events(&fixture, &state).await;
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);

    for arguments in [json!({"extra": true}), json!([]), json!(null)] {
        let (_sender, cancellation) = Cancellation::channel();
        let output = reset.execute(arguments, &mut context, cancellation).await;
        assert!(output.error, "{:?}", output.content);
        assert!(
            output.private_events.is_empty(),
            "invalid arguments cannot reset state"
        );
    }
    let (sender, cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    let output = reset.execute(json!({}), &mut context, cancellation).await;
    assert!(output.error, "{:?}", output.content);
    assert!(output.private_events.is_empty());

    let (_sender, cancellation) = Cancellation::channel();
    let output = reset.execute(json!({}), &mut context, cancellation).await;
    assert!(output.error, "{:?}", output.content);
    assert!(
        output.private_events.is_empty(),
        "missing worker cannot publish a reset checkpoint"
    );
    assert!(!format!("{:?}", output.content).contains("external tool effects may have occurred"));
    drop(context);
    assert_eq!(load_snapshot(&fixture, &state, None).await, original);
    assert_eq!(
        load_snapshot(&fixture, &state, Some(original_id)).await,
        original
    );
    let after_events = load_events(&fixture, &state).await;
    assert_eq!(
        serde_json::to_value(before_events).unwrap(),
        serde_json::to_value(after_events).unwrap()
    );

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_reset_clears_python_state_but_preserves_history_and_restart_barrier() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary.clone()));
    let mut state = fixture.state(tools(&engine), &[]).await;
    install_functions(&mut state);
    state = invoke(
        &fixture,
        state,
        "seed",
        "codemode",
        json!({"code": "import asyncio\nimport math\nx = 41\nitems = [1, 2, 3]\ndef old_function():\n    return x\narchive_alias = prior_results\nprint('old output')"}),
    ).await;
    let (seed_id, error, text) = tool_result(&fixture, &state, "seed").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "old output");
    let old_checkpoint = load_snapshot(&fixture, &state, None).await;
    let before_events = load_events(&fixture, &state).await;

    state = invoke(&fixture, state, "reset", "reset_codemode", json!({})).await;
    let (reset_id, error, text) = tool_result(&fixture, &state, "reset").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), RESET_MESSAGE);
    let fresh_checkpoint = load_snapshot(&fixture, &state, None).await;
    assert_ne!(fresh_checkpoint, old_checkpoint);
    assert_eq!(
        load_snapshot(&fixture, &state, Some(seed_id)).await,
        old_checkpoint
    );
    assert_eq!(
        load_snapshot(&fixture, &state, Some(reset_id)).await,
        fresh_checkpoint
    );
    let after_events = load_events(&fixture, &state).await;
    assert_eq!(after_events.len(), before_events.len() + 4);
    assert_eq!(
        serde_json::to_value(&after_events[..before_events.len()]).unwrap(),
        serde_json::to_value(&before_events).unwrap(),
        "reset must not change any existing public result or event"
    );
    assert_eq!(
        after_events
            .iter()
            .filter(|event| matches!(event.kind, EventType::ToolResult(_)))
            .count(),
        2
    );

    // Replacing the engine must still honor the committed empty-state barrier.
    // It must not restore the older nonempty checkpoint or lose archive access.
    engine.close().await;
    let restarted = Arc::new(MontyEngine::new(binary));
    state.tools = tools(&restarted).into();
    state = invoke(
        &fixture,
        state,
        "after-restart",
        "codemode",
        json!({"code": "try:\n    x\nexcept NameError:\n    print('x absent')\ntry:\n    items\nexcept NameError:\n    print('items absent')\ntry:\n    old_function()\nexcept NameError:\n    print('function absent')\ntry:\n    math\nexcept NameError:\n    print('math absent')\ntry:\n    asyncio\nexcept NameError:\n    print('asyncio absent')\ntry:\n    archive_alias()\nexcept NameError:\n    print('alias absent')\ntry:\n    reset_codemode()\nexcept NameError:\n    print('reset is native only')\nprint(prior_results(0).strip())\nprint(prior_results() == prior_results(0))"}),
    ).await;
    let (_, error, text) = tool_result(&fixture, &state, "after-restart").await;
    assert!(!error, "{text}");
    assert_eq!(
        text.trim(),
        "x absent\nitems absent\nfunction absent\nmath absent\nasyncio absent\nalias absent\nreset is native only\nold output\nTrue"
    );
    assert_eq!(
        load_snapshot(&fixture, &state, Some(seed_id)).await,
        old_checkpoint
    );
    assert_eq!(
        load_snapshot(&fixture, &state, Some(reset_id)).await,
        fresh_checkpoint
    );

    drop(state);
    restarted.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_reset_recovers_from_corrupt_latest_checkpoint_without_restoring_it() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture.state(tools(&engine), &[]).await;
    install_functions(&mut state);
    state = invoke(
        &fixture,
        state,
        "seed",
        "codemode",
        json!({"code": "x = 9\nprint('good old output')"}),
    )
    .await;
    let (seed_id, error, text) = tool_result(&fixture, &state, "seed").await;
    assert!(!error, "{text}");
    let old_checkpoint = load_snapshot(&fixture, &state, None).await;
    let corrupt = b"not a valid Monty checkpoint\0\xff";
    let corrupt_id = inject_snapshot(&mut state, "corrupt-anchor", corrupt).await;
    assert_eq!(load_snapshot(&fixture, &state, None).await, corrupt);

    state = invoke(
        &fixture,
        state,
        "cannot-restore",
        "codemode",
        json!({"code": "print('must not execute')"}),
    )
    .await;
    let (_, error, text) = tool_result(&fixture, &state, "cannot-restore").await;
    assert!(error, "{text}");
    assert!(text.contains("Cannot restore checkpoint"), "{text}");
    assert!(!text.contains("must not execute"), "{text}");
    assert_eq!(load_snapshot(&fixture, &state, None).await, corrupt);

    state = invoke(
        &fixture,
        state,
        "reset-corrupt",
        "reset_codemode",
        json!({}),
    )
    .await;
    let (reset_id, error, text) = tool_result(&fixture, &state, "reset-corrupt").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), RESET_MESSAGE);
    let fresh_checkpoint = load_snapshot(&fixture, &state, None).await;
    assert_ne!(fresh_checkpoint, corrupt);
    assert_eq!(
        load_snapshot(&fixture, &state, Some(seed_id)).await,
        old_checkpoint
    );
    assert_eq!(
        load_snapshot(&fixture, &state, Some(corrupt_id)).await,
        corrupt
    );
    assert_eq!(
        load_snapshot(&fixture, &state, Some(reset_id)).await,
        fresh_checkpoint
    );

    state = invoke(
        &fixture,
        state,
        "recovered",
        "codemode",
        json!({"code": "try:\n    x\nexcept NameError:\n    print('old x absent')\nprint(6 * 7)\nprint(prior_results(0).strip())"}),
    ).await;
    let (_, error, text) = tool_result(&fixture, &state, "recovered").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "old x absent\n42\ngood old output");
    assert_eq!(
        load_snapshot(&fixture, &state, Some(corrupt_id)).await,
        corrupt
    );

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_reset_is_a_same_inference_barrier_and_forks_keep_their_state() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary.clone()));
    let mut state = fixture.state(
        tools(&engine),
        &[
            ("set", "codemode", json!({"code": "x = 17\nprint('before reset')"})),
            ("reset", "reset_codemode", json!({})),
            ("read", "codemode", json!({"code": "try:\n    x\nexcept NameError:\n    print('fresh state')\nprint(prior_results().strip())\ntry:\n    prior_results(1)\nexcept IndexError:\n    print('reset is not archived')"})),
        ],
    ).await;
    install_functions(&mut state);
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    let (set_id, error, text) = tool_result(&fixture, &state, "set").await;
    assert!(!error, "{text}");
    let (reset_id, error, text) = tool_result(&fixture, &state, "reset").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), RESET_MESSAGE);
    let (_, error, text) = tool_result(&fixture, &state, "read").await;
    assert!(!error, "{text}");
    assert_eq!(
        text.trim(),
        "fresh state\nbefore reset\nreset is not archived"
    );
    let old_checkpoint = load_snapshot(&fixture, &state, Some(set_id)).await;
    let fresh_checkpoint = load_snapshot(&fixture, &state, Some(reset_id)).await;
    assert_ne!(old_checkpoint, fresh_checkpoint);

    for (cutoff, expected_snapshot, code, expected_text) in [
        (
            set_id,
            &old_checkpoint,
            "print(x)\nprint(prior_results().strip())",
            "17\nbefore reset",
        ),
        (
            reset_id,
            &fresh_checkpoint,
            "try:\n    x\nexcept NameError:\n    print('fresh fork')\nprint(prior_results().strip())",
            "fresh fork\nbefore reset",
        ),
    ] {
        let mut cursor = fixture.database.write().await.unwrap();
        let fork = cursor
            .fork_session(
                state.session.id,
                crate::sessions::SessionFork {
                    point: crate::sessions::ForkPoint::ThroughEvent {
                        agent_id: state.agent.id,
                        event_id: cutoff,
                    },
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        let child_id = fork
            .agents
            .iter()
            .find(|agent| agent.forked_from == Some(state.agent.id))
            .unwrap()
            .id;
        cursor.commit().await.unwrap();
        let mut child = AgentManagerState::load_persisted(
            fixture.database.clone(),
            fixture.publication.clone(),
            child_id,
        )
        .await
        .unwrap();
        install_functions(&mut child);
        assert_eq!(
            &load_snapshot(&fixture, &child, None).await,
            expected_snapshot
        );
        let probe = fixture.probes.handle(child.session.probe_id).await.unwrap();
        let context =
            AgentContext::invocation(child.invocation_snapshot(), &probe, &fixture.runtime.handle);
        assert_eq!(
            context
                .load_latest_private_event(PrivateEventKind::CodeModeSnapshot)
                .await
                .unwrap(),
            Some(PrivateEvent::CodeModeSnapshot(expected_snapshot.clone()))
        );
        // The fork inherits pending native call announcements from that
        // inference. Execute directly to inspect its chosen checkpoint without
        // allowing those unrelated inherited calls to run again.
        let fork_engine = MontyEngine::new(binary.clone());
        let (_sender, mut cancellation) = Cancellation::channel();
        let output = fork_engine.execute(code, &context, &mut cancellation).await;
        assert!(!output.error, "{}", output.text);
        assert_eq!(output.text.trim(), expected_text);
        assert!(output.snapshot.is_some());
        fork_engine.close().await;
        drop(context);
        drop(child);
    }

    drop(state);
    engine.close().await;
    fixture.close().await;
}
