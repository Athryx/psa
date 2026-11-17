//! Prior-result retrieval reads committed history, not Python checkpoint state.

use super::*;
use crate::components::{
    bundles::codemode::MontyEngine,
    codemode_functions::{
        CodeModeFunction, CodeModeFunctionKind, CodeModeFunctionResult, prior_results::PriorResults,
    },
    tools::CodeMode,
};
use crate::db::NewEvent;
use llm_api::api::ContentBlock;
use serde_json::{Value, json};

fn call_events(inference_id: u64, id: &str, name: &str, arguments: Value) -> Vec<NewEvent> {
    vec![
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
    ]
}

fn completed_call_events(
    inference_id: u64,
    id: &str,
    name: &str,
    text: &str,
    error: bool,
) -> Vec<NewEvent> {
    let mut events = call_events(inference_id, id, name, json!({"code": "pass"}));
    events.push(NewEvent::new(EventType::ToolResult(ToolResult {
        delivery: None,
        call_id: id.into(),
        content: prompts::text(text),
        error,
        metadata: Default::default(),
    })));
    events
}

async fn append_result(
    state: &mut AgentManagerState,
    id: &str,
    name: &str,
    text: &str,
    error: bool,
) -> EventId {
    let events = completed_call_events(state.agent.next_event_id.0, id, name, text, error);
    state
        .persist_events(&events)
        .await
        .unwrap()
        .last()
        .unwrap()
        .id
}

async fn retrieve(context: &mut AgentContext<'_>, arguments: Value) -> CodeModeFunctionResult {
    let (_sender, cancellation) = Cancellation::channel();
    CodeModeFunctionKind::PriorResults(PriorResults)
        .execute(arguments, context, cancellation)
        .await
}

fn text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("unexpected result content: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn prior_results_empty_history_argument_errors_and_precancellation() {
    let fixture = Fixture::new().await;
    let state = fixture.state(vec![], &[]).await;
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);

    for arguments in [json!({}), json!({"index": 0}), json!({"index": -1})] {
        let error = retrieve(&mut context, arguments).await.unwrap_err();
        assert_eq!(error.exception_type, "IndexError");
        assert!(error.message.contains("out of range"), "{error:?}");
    }
    for arguments in [
        json!({"index": null}),
        json!({"index": "-1"}),
        json!({"index": true}),
        json!({"index": 1.5}),
        json!({"index": u64::MAX}),
        json!({"index": -1, "extra": true}),
        json!([]),
        json!(null),
    ] {
        let error = retrieve(&mut context, arguments).await.unwrap_err();
        assert_eq!(error.exception_type, "TypeError", "{error:?}");
    }

    let (sender, cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    let error = CodeModeFunctionKind::PriorResults(PriorResults)
        .execute(
            json!({"index": "invalid even before decoding"}),
            &mut context,
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(error.exception_type, "RuntimeError");
    assert!(error.message.contains("cancelled"), "{error:?}");

    drop(context);
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn prior_results_pages_mixed_history_with_positive_negative_and_frozen_indices() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let mut events = Vec::new();
    let mut expected = Vec::new();
    for index in 0..140 {
        let text = if index == 139 {
            "last output\n[output truncated]\nREPL state is unchanged (changes from this call were discarded); external tool effects may have occurred.\nValueError: archived failure".to_owned()
        } else {
            format!("prior output {index}")
        };
        let id = format!("prior-{index}");
        events.extend(completed_call_events(
            1000 + index * 2,
            &id,
            "codemode",
            &text,
            index % 2 != 0,
        ));
        expected.push(text);
        if index % 7 == 0 {
            events.extend(completed_call_events(
                1001 + index * 2,
                &format!("native-{index}"),
                "noop",
                "native results are not part of the archive",
                false,
            ));
        }
    }
    events.push(NewEvent::new(EventType::Result(
        psa::api::AgentResult::Text("durable agent artifact is not a codemode result".into()),
    )));
    events.push(status_event(AgentState::Idle));
    events.push(status_event(AgentState::Running));
    state.persist_events(&events).await.unwrap();
    assert!(state.context().last_event_id.0 > 128 * 4);

    // The current invocation has a committed call, but no committed result yet.
    let events = call_events(
        2000,
        "current-call",
        "codemode",
        json!({"code": "prior_results()"}),
    );
    state.persist_events(&events).await.unwrap();
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    // Append after taking the invocation snapshot. Both negative-index passes
    // and the final content lookup must stay within the original boundary.
    state
        .persist_events(&[NewEvent::new(EventType::ToolResult(ToolResult {
            delivery: None,
            call_id: "current-call".into(),
            content: prompts::text("late current result must remain invisible"),
            error: false,
            metadata: Default::default(),
        }))])
        .await
        .unwrap();
    append_result(
        &mut state,
        "late-call",
        "codemode",
        "late committed result",
        false,
    )
    .await;

    assert_eq!(
        retrieve(&mut context, json!({})).await.unwrap(),
        json!(expected[139])
    );
    for (index, expected_index) in [
        (0, 0),
        (1, 1),
        (31, 31),
        (32, 32),
        (127, 127),
        (139, 139),
        (-1, 139),
        (-2, 138),
        (-128, 12),
        (-140, 0),
    ] {
        assert_eq!(
            retrieve(&mut context, json!({"index": index}))
                .await
                .unwrap(),
            json!(expected[expected_index]),
            "index {index}"
        );
    }
    for index in [140, -141, i64::MIN, i64::MAX] {
        let error = retrieve(&mut context, json!({"index": index}))
            .await
            .unwrap_err();
        assert_eq!(
            error.exception_type, "IndexError",
            "index {index}: {error:?}"
        );
    }
    drop(context);

    let mut latest_context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    assert_eq!(
        retrieve(&mut latest_context, json!({})).await.unwrap(),
        json!("late committed result")
    );
    drop(latest_context);
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn prior_results_handles_reused_call_ids_across_compacted_contexts() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    append_result(&mut state, "reused", "codemode", "old codemode", false).await;
    for (name, text, error) in [
        ("noop", "native reuse is not codemode", false),
        ("codemode", "new codemode failure", true),
    ] {
        state
            .persist_events(&[
                NewEvent::new(EventType::NewContext),
                NewEvent::new(EventType::Model(support::model_config())),
                NewEvent::new(EventType::Compaction(Compaction {
                    summary: CompactionSummary::Text(
                        "compacted summary, not archived output".into(),
                    ),
                    usage: Default::default(),
                })),
            ])
            .await
            .unwrap();
        append_result(&mut state, "reused", name, text, error).await;
    }
    // An abandoned codemode call must not claim a later native result that
    // happens to reuse its provider call id after context replacement.
    let events = call_events(2900, "stale-id", "codemode", json!({"code": "pass"}));
    state.persist_events(&events).await.unwrap();
    state
        .persist_events(&[
            NewEvent::new(EventType::NewContext),
            NewEvent::new(EventType::Model(support::model_config())),
        ])
        .await
        .unwrap();
    append_result(
        &mut state,
        "stale-id",
        "noop",
        "native overwrote stale codemode call",
        false,
    )
    .await;

    let mut content = prompts::text("first block\n[output truncated]");
    content.extend(prompts::text(
        "second block\nexternal tool effects may have occurred",
    ));
    let mut events = completed_call_events(3000, "blocks", "codemode", "placeholder", true);
    let EventType::ToolResult(result) = &mut events.last_mut().unwrap().kind else {
        unreachable!();
    };
    result.content = content;
    state.persist_events(&events).await.unwrap();
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    for (index, expected) in [
        (0, "old codemode"),
        (1, "new codemode failure"),
        (-2, "new codemode failure"),
        (
            -1,
            "first block\n[output truncated]\nsecond block\nexternal tool effects may have occurred",
        ),
    ] {
        assert_eq!(
            retrieve(&mut context, json!({"index": index}))
                .await
                .unwrap(),
            json!(expected)
        );
    }
    assert_eq!(
        retrieve(&mut context, json!({"index": 3}))
            .await
            .unwrap_err()
            .exception_type,
        "IndexError"
    );

    drop(context);
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn prior_results_respects_fork_cutoffs_and_reads_before_compaction() {
    let fixture = Fixture::new().await;
    let mut parent = fixture.state(vec![], &[]).await;
    let cutoff = append_result(
        &mut parent,
        "inherited",
        "codemode",
        "inherited output",
        false,
    )
    .await;
    append_result(
        &mut parent,
        "excluded",
        "codemode",
        "parent output after cutoff",
        true,
    )
    .await;
    let mut cursor = fixture.database.cursor().await.unwrap();
    let fork = cursor
        .fork_session(
            parent.session.id,
            crate::sessions::SessionFork {
                point: crate::sessions::ForkPoint::ThroughEvent {
                    agent_id: parent.agent.id,
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
        .find(|agent| agent.parent_id == Some(parent.agent.id))
        .unwrap()
        .id;
    cursor.commit().await.unwrap();
    append_result(
        &mut parent,
        "parent-later",
        "codemode",
        "later parent output",
        false,
    )
    .await;
    let mut child = AgentManagerState::load_persisted(
        fixture.database.clone(),
        fixture.publication.clone(),
        child_id,
    )
    .await
    .unwrap();
    child
        .persist_events(&[
            NewEvent::new(EventType::NewContext),
            NewEvent::new(EventType::Model(support::model_config())),
            NewEvent::new(EventType::Compaction(Compaction {
                summary: CompactionSummary::Text("summary of inherited context".into()),
                usage: Default::default(),
            })),
        ])
        .await
        .unwrap();
    append_result(&mut child, "child-only", "codemode", "child output", true).await;
    let probe = fixture.probes.handle(child.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(child.invocation_snapshot(), &probe, &fixture.runtime.handle);
    for (index, expected) in [
        (0, "inherited output"),
        (1, "child output"),
        (-2, "inherited output"),
        (-1, "child output"),
    ] {
        assert_eq!(
            retrieve(&mut context, json!({"index": index}))
                .await
                .unwrap(),
            json!(expected)
        );
    }
    assert_eq!(
        retrieve(&mut context, json!({"index": 2}))
            .await
            .unwrap_err()
            .exception_type,
        "IndexError"
    );

    drop(context);
    drop(child);
    drop(parent);
    fixture.close().await;
}

async fn invoke(
    fixture: &Fixture,
    mut state: AgentManagerState,
    id: &str,
    code: &str,
) -> AgentManagerState {
    let events = call_events(
        state.agent.next_event_id.0,
        id,
        "codemode",
        json!({"code": code}),
    );
    state.persist_events(&events).await.unwrap();
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    state
}

async fn tool_result(fixture: &Fixture, state: &AgentManagerState, id: &str) -> (bool, String) {
    let mut cursor = fixture.database.cursor().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.rollback().await.unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventType::ToolResult(result) if result.call_id == id => Some(result),
            _ => None,
        })
        .expect("codemode result persisted");
    (result.error, text_content(&result.content))
}

#[tokio::test]
async fn real_worker_prior_results_is_synchronous_archived_and_restart_safe() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary.clone()));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions = crate::components::codemode_functions::listed_entries([
        CodeModeFunctionKind::PriorResults(PriorResults),
    ]);
    let mut archive = Vec::new();
    for index in 0..20 {
        let mut text = format!("archive-{index:02}:");
        text.push_str(&"a".repeat(12_000 - text.len()));
        archive.extend(completed_call_events(
            4000 + index,
            &format!("seed-{index}"),
            "codemode",
            &text,
            index % 2 != 0,
        ));
    }
    state.persist_events(&archive).await.unwrap();
    state = invoke(
        &fixture,
        state,
        "archive-size",
        "f = prior_results\nprint(len(prior_results()))\nprint(len(prior_results(-1)))\nprint(len(prior_results(index=0)))",
    ).await;
    let (error, text) = tool_result(&fixture, &state, "archive-size").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "12000\n12000\n12000");
    let mut cursor = fixture.database.cursor().await.unwrap();
    let snapshot = cursor
        .load_latest_private_event(
            state.agent.id,
            None,
            crate::db::PrivateEventKind::CodeModeSnapshot,
        )
        .await
        .unwrap()
        .unwrap();
    let crate::db::PrivateEvent::CodeModeSnapshot(bytes) = snapshot else {
        panic!("expected a codemode snapshot");
    };
    assert!(
        bytes.len() < 8 * 1024,
        "240k of archived results must not enter the checkpoint: {} bytes",
        bytes.len()
    );
    cursor.rollback().await.unwrap();

    state = invoke(
        &fixture,
        state,
        "archived-failure",
        "print('failing output')\nraise ValueError('archive failure')",
    )
    .await;
    let (error, failed_text) = tool_result(&fixture, &state, "archived-failure").await;
    assert!(error, "{failed_text}");
    assert!(
        failed_text.contains("failing output") && failed_text.contains("archive failure"),
        "{failed_text}"
    );
    state = invoke(
        &fixture,
        state,
        "retrieve-failure",
        "print(f() == prior_results(-1))\nprint('failing output' in prior_results())\nprint('archive failure' in prior_results())",
    ).await;
    let (error, text) = tool_result(&fixture, &state, "retrieve-failure").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "True\nTrue\nTrue");

    engine.close().await;
    let restarted = Arc::new(MontyEngine::new(binary));
    state.tools = vec![ToolKind::CodeMode(CodeMode::new(restarted.clone()))].into();
    state = invoke(
        &fixture,
        state,
        "after-restart",
        "print(f(0)[:11])\nprint(f() == prior_results())",
    )
    .await;
    let (error, text) = tool_result(&fixture, &state, "after-restart").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "archive-00:\nTrue");

    let exact = "first line\n[output truncated]\nREPL state is unchanged (changes from this call were discarded); external tool effects may have occurred.\nValueError: preserved failure";
    append_result(&mut state, "synthetic-warning", "codemode", exact, true).await;
    state = invoke(
        &fixture,
        state,
        "exact-archive-text",
        "saved = prior_results()\nprint(saved)",
    )
    .await;
    let (error, text) = tool_result(&fixture, &state, "exact-archive-text").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim_end(), exact);

    state = invoke(
        &fixture,
        state,
        "malformed-archive-calls",
        "assert saved.startswith('first line')\nfor index in ['bad', None, True, 1.5]:\n    try:\n        prior_results(index)\n    except TypeError:\n        print('caught type')\n    else:\n        raise ValueError('bad index accepted')\ntry:\n    prior_results(0, 1)\nexcept TypeError:\n    print('caught extra positional')\ntry:\n    prior_results(0, index=1)\nexcept TypeError:\n    print('caught duplicate index')\ntry:\n    prior_results(unexpected=True)\nexcept TypeError:\n    print('caught keyword')\ntry:\n    prior_results(999999)\nexcept IndexError:\n    print('caught bounds')\ntry:\n    not_a_real_function()\nexcept NameError:\n    print('caught unknown')",
    ).await;
    let (error, text) = tool_result(&fixture, &state, "malformed-archive-calls").await;
    assert!(!error, "{text}");
    assert_eq!(
        text.trim(),
        "caught type\ncaught type\ncaught type\ncaught type\ncaught extra positional\ncaught duplicate index\ncaught keyword\ncaught bounds\ncaught unknown"
    );

    drop(state);
    restarted.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn prior_results_returns_saved_full_output_of_truncated_results() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state(vec![], &[]).await;
    let full: String = (0..4_000).map(|line| format!("line {line}\n")).collect();

    append_result(&mut state, "short", "codemode", "short output", false).await;
    let call = call_events(
        state.agent.next_event_id.0,
        "long",
        "codemode",
        json!({"code": "pass"}),
    );
    state.persist_events(&call).await.unwrap();
    state
        .persist_event_batch(vec![
            EventBatchItem::Public(NewEvent::new(EventType::ToolResult(ToolResult {
                delivery: None,
                call_id: "long".into(),
                content: prompts::text("line 0\n[output truncated]\nline 3999\n"),
                error: false,
                metadata: Default::default(),
            }))),
            EventBatchItem::Private(crate::db::PrivateEvent::CodeModeOutput(full.clone())),
        ])
        .await
        .unwrap();
    append_result(&mut state, "after", "codemode", "after output", false).await;

    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let mut context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    assert_eq!(
        retrieve(&mut context, json!({"index": 1})).await.unwrap(),
        Value::String(full.clone())
    );
    assert_eq!(
        retrieve(&mut context, json!({"index": -2})).await.unwrap(),
        Value::String(full)
    );
    // Untruncated neighbours are unaffected by the saved output.
    assert_eq!(
        retrieve(&mut context, json!({"index": 0})).await.unwrap(),
        Value::String("short output".into())
    );
    assert_eq!(
        retrieve(&mut context, json!({})).await.unwrap(),
        Value::String("after output".into())
    );

    drop(context);
    drop(state);
    fixture.close().await;
}
