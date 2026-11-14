//! Full worker/tool/checkpoint integration using an explicitly configured worker.

use super::*;
use crate::components::{
    bundles::codemode::MontyEngine,
    codemode_functions::CodeModeFunctionKind,
    codemode_functions::prior_results::PriorResults,
    tools::{CodeMode, Noop},
};
use llm_api::api::ContentBlock;
use serde_json::json;

async fn append_tool_call(
    state: &mut AgentManagerState,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) {
    let inference_id = state.agent.next_event_id.0 as u64;
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

async fn invoke_codemode(
    fixture: &Fixture,
    mut state: AgentManagerState,
    id: &str,
    code: &str,
) -> AgentManagerState {
    append_tool_call(&mut state, id, "codemode", json!({"code": code})).await;
    let (controls, run) = fixture.run(state).await;
    let state = finish(run).await;
    drop(controls);
    state
}

async fn load_tool_result(
    fixture: &Fixture,
    state: &AgentManagerState,
    id: &str,
) -> (bool, String) {
    let mut cursor = fixture.database.cursor().await.unwrap();
    let events = cursor.load_all_events(state.agent.id).await.unwrap();
    cursor.rollback().await.unwrap();
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
            ContentBlock::Text(text) => text.text.as_str(),
            other => panic!("unexpected content: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (result.error, text)
}

#[tokio::test]
async fn real_worker_persists_snapshots_errors_output_and_registered_functions() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions = crate::components::codemode_functions::listed_entries([
        CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
        CodeModeFunctionKind::PriorResults(PriorResults),
    ]);

    state = invoke_codemode(&fixture, state, "first", "x = 41\nprint('first')").await;
    let (error, text) = load_tool_result(&fixture, &state, "first").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "first");
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot,
            )
            .await
            .unwrap()
            .is_some()
    );
    cursor.rollback().await.unwrap();

    state = invoke_codemode(&fixture, state, "second", "print(x + 1)").await;
    let (error, text) = load_tool_result(&fixture, &state, "second").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "42");

    state = invoke_codemode(&fixture, state, "final-expression", "x").await;
    let (error, text) = load_tool_result(&fixture, &state, "final-expression").await;
    assert!(!error, "{text}");
    assert!(
        text.is_empty(),
        "final expression must not be printed: {text}"
    );

    state = invoke_codemode(
        &fixture,
        state,
        "exception",
        "x = 99\nprint('before error')\nraise ValueError('broken')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "exception").await;
    assert!(error, "{text}");
    assert!(text.contains("before error"), "{text}");
    assert!(
        text.contains("ValueError") && text.contains("broken"),
        "{text}"
    );

    state = invoke_codemode(&fixture, state, "after-error", "print(x)").await;
    let (error, text) = load_tool_result(&fixture, &state, "after-error").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "99");

    state = invoke_codemode(&fixture, state, "noop", "import asyncio\nasync def main():\n    result = await noop()\n    print('noop result:', repr(result))\nasyncio.run(main())").await;
    let (error, text) = load_tool_result(&fixture, &state, "noop").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "noop result: ''");

    state = invoke_codemode(
        &fixture,
        state,
        "bad-arguments",
        "asyncio.run(noop(unexpected=True))",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "bad-arguments").await;
    assert!(error, "{text}");
    assert!(text.contains("RuntimeError"), "{text}");

    state = invoke_codemode(
        &fixture,
        state,
        "caught-host-error",
        "async def handle_error():\n    try:\n        await noop(unexpected=True)\n    except RuntimeError:\n        print('caught host error')\nasyncio.run(handle_error())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "caught-host-error").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "caught host error");

    state = invoke_codemode(
        &fixture,
        state,
        "gather",
        "async def both():\n    return await asyncio.gather(noop(), noop())\nprint(asyncio.run(both()))",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "gather").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "['', '']");

    state = invoke_codemode(&fixture, state, "large-output", "print('a' * 13000)").await;
    let (error, text) = load_tool_result(&fixture, &state, "large-output").await;
    assert!(!error, "{text}");
    assert!(text.contains("[output truncated]"));
    assert!(text.chars().count() <= 12_000);

    // The model sees a truncated view, but the full output stays retrievable.
    state = invoke_codemode(
        &fixture,
        state,
        "recover",
        "full = prior_results(-1)\nprint(len(full), full.count('a'))",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "recover").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "13001 13000");

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_unknown_calls_raise_name_error_without_awaiting() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions =
        crate::components::codemode_functions::listed_entries([CodeModeFunctionKind::Tool(
            ToolKind::Noop(Noop),
        )]);

    for (index, expression) in [
        "not_a_real_function(1, 2)",
        "dir()",
        "globals()",
        "vars()",
        "__import__('math')",
        "await not_a_real_function()",
    ]
    .into_iter()
    .enumerate()
    {
        let code = format!(
            "import asyncio\nasync def check():\n    try:\n        {expression}\n    except NameError as error:\n        print('caught:', error)\n    else:\n        raise ValueError('unknown call did not raise immediately')\nasyncio.run(check())"
        );
        let id = format!("caught-unknown-{index}");
        state = invoke_codemode(&fixture, state, &id, &code).await;
        let (error, text) = load_tool_result(&fixture, &state, &id).await;
        assert!(!error, "{expression}: {text}");
        assert!(
            text.contains("caught:") && text.contains("is not defined"),
            "{text}"
        );
        assert!(!text.contains("unresolved host calls"), "{text}");
    }

    state = invoke_codemode(
        &fixture,
        state,
        "uncaught-unknown",
        "saved_before_unknown = 42\nprint('before unknown')\nnot_a_real_function()\nprint('must not execute')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "uncaught-unknown").await;
    assert!(error, "{text}");
    assert!(text.contains("before unknown"), "{text}");
    assert!(
        text.contains("NameError: name 'not_a_real_function' is not defined"),
        "{text}"
    );
    assert!(!text.contains("must not execute"), "{text}");
    assert!(
        !text.contains("changes from this call were discarded"),
        "{text}"
    );

    state = invoke_codemode(
        &fixture,
        state,
        "after-unknown",
        "print(saved_before_unknown)",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "after-unknown").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "42");

    state = invoke_codemode(
        &fixture,
        state,
        "unawaited-known-call",
        "print('before dangling call')\nnoop()\nprint('after dangling call')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "unawaited-known-call").await;
    assert!(error, "{text}");
    assert!(text.contains("noop") && text.contains("await"), "{text}");
    assert!(text.contains("before dangling call"), "{text}");
    assert!(text.contains("after dangling call"), "{text}");
    assert!(text.contains("REPL state is unchanged"), "{text}");
    assert!(
        !text.contains("external tool effects may have occurred"),
        "an unawaited host function was never started: {text}"
    );

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_sleeps_inline_and_concurrently_with_registered_functions() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions =
        crate::components::codemode_functions::listed_entries([CodeModeFunctionKind::Tool(
            ToolKind::Noop(Noop),
        )]);

    state = invoke_codemode(
        &fixture,
        state,
        "inline-sleep",
        "import time\nimport asyncio\nprint('before sleep')\nprint(repr(time.sleep(0.01)))\nprint('after sleep')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "inline-sleep").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "before sleep\nNone\nafter sleep");

    state = invoke_codemode(
        &fixture,
        state,
        "concurrent-sleeps",
        "async def sleeper(name, delay):\n    print(name + ' start')\n    result = await asyncio.sleep(delay)\n    print(name + ' end')\n    return result\nasync def host():\n    print('host start')\n    result = await noop()\n    print('host end')\n    return result\nasync def together():\n    print(await asyncio.gather(sleeper('slow', 0.4), sleeper('fast', 0.02), host()))\nasyncio.run(together())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "concurrent-sleeps").await;
    assert!(!error, "{text}");
    let position = |line| text.lines().position(|value| value == line).unwrap();
    // Ordering, not a tight elapsed-time bound, proves that neither the second
    // sleeper nor the registered host function waits for the first sleep.
    assert!(position("slow start") < position("fast start"), "{text}");
    assert!(position("fast start") < position("fast end"), "{text}");
    assert!(position("fast end") < position("slow end"), "{text}");
    assert!(position("host start") < position("host end"), "{text}");
    assert!(position("host end") < position("slow end"), "{text}");
    assert!(text.contains("[None, None, '']"), "{text}");

    state = invoke_codemode(
        &fixture,
        state,
        "invalid-sleeps",
        "for delay in [-1, -0.5]:\n    try:\n        time.sleep(delay)\n    except ValueError:\n        print('caught inline', delay)\n    else:\n        raise RuntimeError('invalid inline delay accepted')\nasync def invalid_sleeps():\n    try:\n        await asyncio.sleep(float('nan'))\n    except ValueError:\n        print('caught async nan')\n    else:\n        raise RuntimeError('invalid async delay accepted')\n    print(repr(await asyncio.sleep(-1)))\n    print(await asyncio.sleep(0, result='zero token'))\n    print(await asyncio.sleep(0.01, result='positive token'))\nasyncio.run(invalid_sleeps())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "invalid-sleeps").await;
    assert!(!error, "{text}");
    assert_eq!(
        text.trim(),
        "caught inline -1\ncaught inline -0.5\ncaught async nan\nNone\nzero token\npositive token"
    );

    for (id, code, names, host_started) in [
        (
            "unawaited-sleep",
            "print('before unawaited sleep')\nasyncio.sleep(0.01)\nprint('after unawaited sleep')",
            &["asyncio.sleep"][..],
            false,
        ),
        (
            "unawaited-mixed",
            "print('before unawaited sleep')\nnoop()\nasyncio.sleep(0.01)\nprint('after unawaited sleep')",
            &["noop", "asyncio.sleep"][..],
            false,
        ),
        (
            "unawaited-after-host",
            "asyncio.run(noop())\nprint('before unawaited sleep')\nasyncio.sleep(0.01)\nprint('after unawaited sleep')",
            &["asyncio.sleep"][..],
            true,
        ),
    ] {
        state = invoke_codemode(&fixture, state, id, code).await;
        let (error, text) = load_tool_result(&fixture, &state, id).await;
        assert!(error, "{text}");
        assert!(text.contains("before unawaited sleep"), "{text}");
        assert!(text.contains("after unawaited sleep"), "{text}");
        assert!(text.contains("await"), "{text}");
        assert!(text.contains("REPL state is unchanged"), "{text}");
        for name in names {
            assert!(text.contains(name), "missing {name}: {text}");
        }
        assert_eq!(
            text.contains("external tool effects may have occurred"),
            host_started,
            "{text}"
        );
    }

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_sleep_cancellation_and_close_are_prompt_without_host_effects() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let state = fixture.state(vec![], &[]).await;
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);

    for script in [
        "import time\nprint('sleep started')\ntime.sleep(100)\nprint('sleep finished')",
        "import asyncio\nasync def wait():\n    print('sleep started')\n    await asyncio.sleep(100)\n    print('sleep finished')\nasyncio.run(wait())",
        "import asyncio\nasync def wait():\n    print('sleep started')\n    await asyncio.sleep(float('inf'))\n    print('sleep finished')\nasyncio.run(wait())",
    ] {
        for close_engine in [false, true] {
            let engine = MontyEngine::new(binary.clone());
            let (sender, mut cancellation) = Cancellation::channel();
            let mut execution = Box::pin(engine.execute(script, &context, &mut cancellation));
            assert!(
                tokio::time::timeout(Duration::from_secs(1), &mut execution)
                    .await
                    .is_err(),
                "sleep should still be running before cancellation"
            );

            let output = tokio::time::timeout(Duration::from_secs(2), async {
                if close_engine {
                    let (output, ()) = tokio::join!(&mut execution, engine.close());
                    output
                } else {
                    sender.send(()).unwrap();
                    execution.await
                }
            })
            .await
            .expect("cancelling a pure sleep must not wait for its requested delay");
            assert!(output.error, "{}", output.text);
            assert!(output.snapshot.is_none());
            assert!(output.text.contains("sleep started"), "{}", output.text);
            assert!(!output.text.contains("sleep finished"), "{}", output.text);
            assert!(
                output.text.contains("REPL state is unchanged"),
                "{}",
                output.text
            );
            assert!(
                output.text.contains(if close_engine {
                    "shut down"
                } else {
                    "cancelled"
                }),
                "{}",
                output.text
            );
            assert!(
                !output
                    .text
                    .contains("external tool effects may have occurred"),
                "{}",
                output.text
            );
            engine.close().await;
        }
    }

    drop(context);
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_does_not_shorten_sleeps_at_ten_seconds() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let state = fixture.state(vec![], &[]).await;
    let probe = fixture.probes.handle(state.session.probe_id).await.unwrap();
    let context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    let engine = MontyEngine::new(binary);
    let (_sender, mut cancellation) = Cancellation::channel();
    let started = tokio::time::Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        engine.execute(
            "import asyncio\nasync def wait():\n    print(repr(await asyncio.sleep(11)))\nasyncio.run(wait())",
            &context,
            &mut cancellation,
        ),
    )
    .await
    .expect("sleep(11) must complete within the script's wall limit");
    assert!(!output.error, "{}", output.text);
    assert_eq!(output.text.trim(), "None");
    assert!(output.snapshot.is_some());
    assert!(
        started.elapsed() >= Duration::from_secs(11),
        "sleep(11) must not be shortened by Monty's default ten-second cap"
    );

    engine.close().await;
    drop(context);
    drop(state);
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_checkpoint_budget_failure_preserves_output_and_previous_state() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state = invoke_codemode(&fixture, state, "seed-checkpoint", "x = 41").await;
    let (error, text) = load_tool_result(&fixture, &state, "seed-checkpoint").await;
    assert!(!error, "{text}");
    let mut cursor = fixture.database.cursor().await.unwrap();
    let previous = cursor
        .load_latest_private_event(
            state.agent.id,
            None,
            crate::db::PrivateEventKind::CodeModeSnapshot,
        )
        .await
        .unwrap();
    assert!(previous.is_some());
    cursor.rollback().await.unwrap();

    state = invoke_codemode(
        &fixture,
        state,
        "oversized-checkpoint",
        "print('before checkpoint failure')\nx = 99\nlarge = 'z' * (18 * 1024 * 1024)\nprint('large allocation completed')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "oversized-checkpoint").await;
    assert!(error, "{text}");
    assert!(text.contains("before checkpoint failure"), "{text}");
    assert!(text.contains("REPL state is unchanged"), "{text}");
    assert!(text.contains("64 MiB"), "{text}");
    if text.contains("Saved REPL state is") {
        assert!(text.contains("16 MiB"), "{text}");
        assert!(
            text.contains("bytes"),
            "serialized size must be reported: {text}"
        );
        assert!(
            text.split_whitespace().any(|part| {
                part.trim_matches(|character: char| !character.is_ascii_digit())
                    .parse::<usize>()
                    .is_ok_and(|size| size > crate::db::MAX_CODEMODE_SNAPSHOT_BYTES)
            }),
            "serialized byte count must exceed the checkpoint budget: {text}"
        );
        assert!(text.contains("large allocation completed"), "{text}");
    } else {
        // Worker versions may hit their interpreter/dump memory limit before
        // returning the oversized serialized checkpoint to the host.
        assert!(
            text.contains("MemoryError") || text.contains("Cannot save REPL state"),
            "expected a detailed checkpoint-size or dump-memory error: {text}"
        );
    }
    assert!(
        !text.contains("external tool effects may have occurred"),
        "{text}"
    );
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
        previous,
        "failed checkpoint must not replace the previous bytes"
    );
    cursor.rollback().await.unwrap();

    state = invoke_codemode(
        &fixture,
        state,
        "after-checkpoint-failure",
        "print(x)\ntry:\n    large\nexcept NameError:\n    print('large is absent')\nelse:\n    raise ValueError('failed turn leaked into checkpoint')",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "after-checkpoint-failure").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "41\nlarge is absent");

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn preflight_missing_worker_and_oversized_source_do_not_claim_host_effects() {
    let fixture = Fixture::new().await;
    let missing = fixture
        ._directory
        .path()
        .join("never-created")
        .join("monty-worker");
    assert!(!missing.exists());
    let engine = Arc::new(MontyEngine::new(missing));
    let mut state = fixture
        .state(vec![ToolKind::CodeMode(CodeMode::new(engine.clone()))], &[])
        .await;
    state.codemode_functions =
        crate::components::codemode_functions::listed_entries([CodeModeFunctionKind::Tool(
            ToolKind::Noop(Noop),
        )]);

    let oversized_source = "#".repeat(256 * 1024 + 1);
    for (id, script, reason) in [
        (
            "missing-worker",
            "print('must not execute')\nnoop()",
            "Cannot initialize interpreter",
        ),
        (
            "oversized-source",
            oversized_source.as_str(),
            "Script exceeds 256 KiB",
        ),
    ] {
        state = invoke_codemode(&fixture, state, id, script).await;
        let (error, text) = load_tool_result(&fixture, &state, id).await;
        assert!(error, "{text}");
        assert!(text.contains(reason), "{text}");
        assert!(text.contains("did not run"), "{text}");
        assert!(text.contains("REPL state is unchanged"), "{text}");
        assert!(!text.contains("must not execute"), "{text}");
        assert!(
            !text.contains("external tool effects may have occurred"),
            "{text}"
        );
    }
    let mut cursor = fixture.database.cursor().await.unwrap();
    assert!(
        cursor
            .load_latest_private_event(
                state.agent.id,
                None,
                crate::db::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap()
            .is_none()
    );
    cursor.rollback().await.unwrap();

    drop(state);
    engine.close().await;
    fixture.close().await;
}

#[tokio::test]
async fn real_worker_shares_bash_sessions_in_both_directions() {
    if std::env::var_os("PSA_MONTY_WORKER").is_none() {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    }
    let fixture = Fixture::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = crate::http::router(AppState {
        database: fixture.database.clone(),
        publication: fixture.publication.clone(),
        runtime: fixture.runtime.handle.clone(),
        accepting: Arc::new(AtomicBool::new(true)),
        client_key: "test-client-key".into(),
        client_config: psa_api::ClientConfig {
            default_agent_name: "root".into(),
            default_sandbox_image: None,
        },
        probes: fixture.probes.clone(),
    });
    let (stop_server, stopped_server) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped_server.await;
            })
            .await
            .unwrap();
    });
    let mut metadata = support::probe_metadata();
    metadata.id = psa_api::ProbeId(psa_api::probes::Uuid::new_v4());
    metadata.data_dir = fixture._directory.path().join("real-probe");
    let probe_id = metadata.id;
    let config = psa_probe::config::ResolvedConfig {
        metadata,
        server_url: format!("ws://{address}/probes/connect"),
        key: fake_probe::PROBE_KEY.into(),
        sandbox_probe_path: None,
    };
    let (stop_probe, stopped_probe) = oneshot::channel();
    let probe_task = tokio::spawn(psa_probe::run(config, async {
        let _ = stopped_probe.await;
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.probes.is_active(probe_id).await {
            assert!(
                !probe_task.is_finished(),
                "real probe exited before connecting"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let mut state = fixture.state(vec![], &[]).await;
    state.session.probe_id = probe_id;
    state.session.working_directory = fixture._directory.path().to_owned();
    let mut definition = state.definition.clone();
    definition.bundles = vec![
        psa_api::BundleConfig::Bash,
        psa_api::BundleConfig::FileTools { read_only: true },
        psa_api::BundleConfig::CodeMode,
    ];
    let installed = crate::components::bundles::InstalledComponents::install(
        &definition,
        &state.agent,
        &state.session,
    )
    .await
    .unwrap();
    let definitions = installed
        .tools
        .iter()
        .map(|tool| {
            NewEvent::new(EventType::ToolDefinition(psa_api::events::ToolDefinition {
                id: tool.id(),
                definition: tool.tool_definition(),
            }))
        })
        .collect::<Vec<_>>();
    state.persist_events(&definitions).await.unwrap();
    let components = Arc::new(tokio::sync::Mutex::new(installed));
    state.attach_components(components.clone()).await;

    append_tool_call(&mut state, "native-start", "bash", json!({"command": "read -r line; printf 'native-received:%s\\n' \"$line\"", "session_name":"native-session", "yield_time_ms":50})).await;
    let (controls, run) = fixture.run(state).await;
    state = finish(run).await;
    drop(controls);
    let (error, text) = load_tool_result(&fixture, &state, "native-start").await;
    assert!(!error && text.contains("native-session"), "{text}");

    state = invoke_codemode(&fixture, state, "python-interact", "import asyncio\nasync def run():\n    print(await interact_bash(session_name='native-session', stdin='hello from python\\n', yield_time_ms=2000))\nasyncio.run(run())").await;
    let (error, text) = load_tool_result(&fixture, &state, "python-interact").await;
    assert!(
        !error && text.contains("native-received:hello from python"),
        "{text}"
    );

    // A failing command is a result, not an exception; str() is the tool text.
    state = invoke_codemode(
        &fixture,
        state,
        "python-bash-failure",
        "async def fail():\n    r = await bash(command='echo out; exit 3')\n    print(r.exit_code, r.running, r.output.strip())\n    print(repr(r))\n    print(r)\nasyncio.run(fail())",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-bash-failure").await;
    assert!(!error, "{text}");
    assert!(text.starts_with("3 False out\nBashResult("), "{text}");
    assert!(text.contains("exit_code=3"), "{text}");
    assert!(text.contains("Command exited with code 3"), "{text}");

    // Finished sessions stay addressable; only names never used are unknown.
    state = invoke_codemode(
        &fixture,
        state,
        "python-finished",
        "r = await bash(command='exit 4')\nc = await close_bash(session_name=r.session_name)\nprint(c.running, c.exit_code, repr(c.output))\nprint(c)\np = await interact_bash(session_name=r.session_name)\nprint(p.exit_code)\ntry:\n    await interact_bash(session_name=r.session_name, stdin='x')\nexcept RuntimeError as e:\n    print('stdin:', e)\ntry:\n    await close_bash(session_name='never-started')\nexcept RuntimeError as e:\n    print('unknown:', e)",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-finished").await;
    assert!(!error, "{text}");
    assert!(text.starts_with("False 4 ''\n"), "{text}");
    assert!(text.contains("had already finished"), "{text}");
    assert!(text.contains("Command exited with code 4\n4\n"), "{text}");
    assert!(text.contains("stdin: Bash session"), "{text}");
    assert!(text.contains("not accepting stdin"), "{text}");
    assert!(
        text.contains("unknown: Unknown bash session: never-started"),
        "{text}"
    );

    std::fs::write(
        fixture._directory.path().join("lines.txt"),
        "one\ntwo\nthree\n",
    )
    .unwrap();
    state = invoke_codemode(
        &fixture,
        state,
        "python-read",
        "r = await read(path='lines.txt', limit=2)\nprint(r.content.split())\nprint(r.start_line, r.end_line, r.total_lines, r.next_offset)\nprint(r)",
    )
    .await;
    let (error, text) = load_tool_result(&fixture, &state, "python-read").await;
    assert!(!error, "{text}");
    assert!(
        text.starts_with("['one', 'two']\n1 2 4 3\none\ntwo"),
        "{text}"
    );
    assert!(text.contains("Use offset=3 to continue."), "{text}");

    state = invoke_codemode(&fixture, state, "python-start", "async def start():\n    print(await bash(command=\"read -r line; printf 'python-received:%s\\\\n' \\\"$line\\\"\", session_name='python-session', yield_time_ms=50))\nasyncio.run(start())").await;
    let (error, text) = load_tool_result(&fixture, &state, "python-start").await;
    assert!(!error && text.contains("python-session"), "{text}");

    append_tool_call(&mut state, "native-interact", "interact_bash", json!({"session_name":"python-session", "stdin":"hello from native\n", "yield_time_ms":2000})).await;
    let (controls, run) = fixture.run(state).await;
    state = finish(run).await;
    drop(controls);
    let (error, text) = load_tool_result(&fixture, &state, "native-interact").await;
    assert!(
        !error && text.contains("python-received:hello from native"),
        "{text}"
    );

    drop(state);
    components.lock().await.cleanup().await.unwrap();
    let _ = stop_probe.send(());
    tokio::time::timeout(Duration::from_secs(5), probe_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    fixture.close().await;
}
