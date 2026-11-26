//! Full worker/tool/checkpoint integration using an explicitly configured worker.

use super::*;
use crate::codemode_functions::AgentCodeModeHost;
use crate::codemode_functions::ToolFunction;
use crate::{codemode_functions::prior_results::PriorResults, tools::CodeMode};
use psa_codemode::MontyEngine;
use serde_json::json;

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

#[tokio::test]
async fn real_worker_persists_snapshots_errors_output_and_registered_functions() {
    let Some(binary) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping real Monty integration: PSA_MONTY_WORKER is not set");
        return;
    };
    let fixture = Fixture::new().await;
    let engine = Arc::new(MontyEngine::new(binary));
    let mut state = fixture
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[],
        )
        .await;
    state.set_codemode_functions(crate::codemode_functions::listed_entries([
        Box::new(ToolFunction::new(crate::bundles::testing::test_tool(
            "noop",
            psa::api::ToolId::Edit,
        ))) as Box<dyn crate::codemode_functions::CodeModeFunction>,
        Box::new(PriorResults) as Box<dyn crate::codemode_functions::CodeModeFunction>,
    ]));

    state = invoke_codemode(&fixture, state, "first", "x = 41\nprint('first')").await;
    let (error, text) = load_tool_result(&fixture, &state, "first").await;
    assert!(!error, "{text}");
    assert_eq!(text.trim(), "first");
    let mut cursor = fixture.database.read().await.unwrap();
    assert!(
        cursor
            .load_latest_private_event(
                state.agent().id,
                None,
                psa_store::PrivateEventKind::CodeModeSnapshot,
            )
            .await
            .unwrap()
            .is_some()
    );
    cursor.close().await.unwrap();

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
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[],
        )
        .await;
    state.set_codemode_functions(crate::codemode_functions::listed_entries([Box::new(
        ToolFunction::new(crate::bundles::testing::test_tool(
            "noop",
            psa::api::ToolId::Edit,
        )),
    )
        as Box<dyn crate::codemode_functions::CodeModeFunction>]));

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
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[],
        )
        .await;
    state.set_codemode_functions(crate::codemode_functions::listed_entries([Box::new(
        ToolFunction::new(crate::bundles::testing::test_tool(
            "noop",
            psa::api::ToolId::Edit,
        )),
    )
        as Box<dyn crate::codemode_functions::CodeModeFunction>]));

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
    let probe = fixture
        .probes
        .handle(state.session().probe_id)
        .await
        .unwrap();
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
            let host = AgentCodeModeHost::new(&context);
            let mut execution = Box::pin(engine.execute(script, &host, &mut cancellation));
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
    let probe = fixture
        .probes
        .handle(state.session().probe_id)
        .await
        .unwrap();
    let context =
        AgentContext::invocation(state.invocation_snapshot(), &probe, &fixture.runtime.handle);
    let engine = MontyEngine::new(binary);
    let (_sender, mut cancellation) = Cancellation::channel();
    let started = tokio::time::Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        engine.execute(
            "import asyncio\nasync def wait():\n    print(repr(await asyncio.sleep(11)))\nasyncio.run(wait())",
            &AgentCodeModeHost::new(&context),
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
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[],
        )
        .await;
    state = invoke_codemode(&fixture, state, "seed-checkpoint", "x = 41").await;
    let (error, text) = load_tool_result(&fixture, &state, "seed-checkpoint").await;
    assert!(!error, "{text}");
    let mut cursor = fixture.database.read().await.unwrap();
    let previous = cursor
        .load_latest_private_event(
            state.agent().id,
            None,
            psa_store::PrivateEventKind::CodeModeSnapshot,
        )
        .await
        .unwrap();
    assert!(previous.is_some());
    cursor.close().await.unwrap();

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
                    .is_ok_and(|size| size > psa_store::MAX_CODEMODE_SNAPSHOT_BYTES)
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
    let mut cursor = fixture.database.read().await.unwrap();
    assert_eq!(
        cursor
            .load_latest_private_event(
                state.agent().id,
                None,
                psa_store::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap(),
        previous,
        "failed checkpoint must not replace the previous bytes"
    );
    cursor.close().await.unwrap();

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
        .directory
        .path()
        .join("never-created")
        .join("monty-worker");
    assert!(!missing.exists());
    let engine = Arc::new(MontyEngine::new(missing));
    let mut state = fixture
        .state(
            vec![Box::new(CodeMode::new(engine.clone())) as Box<dyn crate::tools::DynTool>],
            &[],
        )
        .await;
    state.set_codemode_functions(crate::codemode_functions::listed_entries([Box::new(
        ToolFunction::new(crate::bundles::testing::test_tool(
            "noop",
            psa::api::ToolId::Edit,
        )),
    )
        as Box<dyn crate::codemode_functions::CodeModeFunction>]));

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
    let mut cursor = fixture.database.read().await.unwrap();
    assert!(
        cursor
            .load_latest_private_event(
                state.agent().id,
                None,
                psa_store::PrivateEventKind::CodeModeSnapshot
            )
            .await
            .unwrap()
            .is_none()
    );
    cursor.close().await.unwrap();

    drop(state);
    engine.close().await;
    fixture.close().await;
}
