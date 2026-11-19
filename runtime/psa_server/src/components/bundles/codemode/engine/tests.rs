use super::*;
use serde_json::json;

#[tokio::test]
async fn worker_path_resolves_explicit_and_path_entries() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("monty");
    tokio::fs::write(&worker, b"worker").await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
    }
    let search_path = std::env::join_paths([temp.path()]).unwrap();
    let expected = tokio::fs::canonicalize(&worker).await.unwrap();
    assert_eq!(
        resolve_worker_path_with_search_path(Path::new("monty"), &search_path)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        resolve_worker_path_with_search_path(&worker, &search_path)
            .await
            .unwrap(),
        expected
    );
    let relative = Path::new(".").join("Cargo.toml");
    assert_eq!(
        resolve_worker_path_with_search_path(&relative, &search_path)
            .await
            .unwrap(),
        tokio::fs::canonicalize(relative).await.unwrap()
    );
    assert!(
        resolve_worker_path_with_search_path(Path::new("missing-worker"), &search_path)
            .await
            .is_err()
    );
}

#[test]
fn wall_deadline_covers_maximum_registered_wait() {
    assert!(WALL_TIMEOUT > Duration::from_secs(600));
    assert_eq!(FEED_TIMEOUT, Duration::from_secs(60));
}

#[test]
fn reset_and_execution_use_the_same_current_repl_configuration() {
    let config = build_repl_config();
    let limits = config.limits.unwrap();
    assert_eq!(limits.max_feed_duration, Some(Duration::from_secs(60)));
    assert_eq!(limits.max_memory, Some(64 * 1024 * 1024));
    assert_eq!(config.os_policy.sleep, SleepMode::System(Duration::MAX));
    assert_eq!(config.print_flush_interval, Some(Duration::ZERO));
}

#[tokio::test]
async fn reset_cancellation_and_shutdown_do_not_initialize_a_worker() {
    let engine = MontyEngine::new("unused-monty-worker");
    let (sender, mut cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    let error = engine.reset(&mut cancellation).await.unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert!(engine.pool.get().is_none());

    engine.close().await;
    let (_sender, mut cancellation) = Cancellation::channel();
    let error = engine.reset(&mut cancellation).await.unwrap_err();
    assert!(error.contains("shut down"), "{error}");
    assert!(engine.pool.get().is_none());
}

#[test]
fn print_cap_counts_unicode_and_truncation_marker() {
    let mut output = BoundedOutput::new(30);
    output.push_str(&"😺".repeat(40));
    let text = output.finish();
    assert_eq!(text.chars().count(), 30);
    assert!(text.contains("[output truncated]"));
    assert!(text.starts_with('😺'));
    assert!(text.ends_with('😺'));

    let mut tiny = BoundedOutput::new(3);
    tiny.push_str("1234");
    assert_eq!(tiny.finish().chars().count(), 3);
}

#[test]
fn print_cap_keeps_unicode_head_and_tail_across_writes() {
    let mut output = BoundedOutput::new(25);
    output.push_str("éabc");
    output.push_str(&"x".repeat(100));
    output.push_str("猫終");
    let text = output.finish();
    assert_eq!(text, "éab\n[output truncated]\n猫終");
    assert_eq!(text.chars().count(), 25);
}

#[test]
fn print_cap_preserves_exact_limit_across_multiple_writes() {
    let mut output = BoundedOutput::new(3);
    output.push_str("é");
    output.push_str("ab");
    output.push_str("");
    assert_eq!(output.finish(), "éab");

    let mut output = BoundedOutput::new(0);
    output.push_str("x");
    assert_eq!(output.finish(), "");
}

#[test]
fn exception_remains_visible_after_print_truncation() {
    let mut output = BoundedOutput::new(MAX_PRINT_CHARS);
    output.push_str(&"é".repeat(MAX_OUTPUT_CHARS));
    let mut text = output.finish();
    append_error_output(
        &mut text,
        &format!("ValueError: {}", "x".repeat(MAX_OUTPUT_CHARS)),
        true,
        &ExecutionProgress {
            script_started: true,
            host_call_started: AtomicBool::new(true),
        },
        MAX_OUTPUT_CHARS,
    );
    assert!(text.contains("[output truncated]"));
    assert!(text.contains("ValueError:"));
    assert!(text.contains("REPL state is unchanged (changes from this call were discarded)"));
    assert!(text.contains("external tool effects may have occurred"));
    assert!(text.chars().count() <= MAX_OUTPUT_CHARS);

    let mut text = String::new();
    append_error_output(
        &mut text,
        "ValueError: failure",
        false,
        &ExecutionProgress::default(),
        MAX_OUTPUT_CHARS,
    );
    assert_eq!(text, "ValueError: failure");
}

#[test]
fn unresolved_calls_keep_exception_and_warn_about_unsaved_state() {
    let calls = BTreeMap::from([(2, "noop".into()), (1, "asyncio.sleep".into())]);
    let error = unresolved_calls_error(&calls, Some("ValueError: example"));
    let mut text = String::new();
    append_error_output(
        &mut text,
        &error,
        true,
        &ExecutionProgress {
            script_started: true,
            host_call_started: AtomicBool::new(true),
        },
        MAX_OUTPUT_CHARS,
    );
    assert!(text.contains("changes from this call were discarded"));
    assert!(text.contains("external tool effects may have occurred"));
    assert!(text.contains("asyncio.sleep() (call 1), noop() (call 2)"));
    assert!(text.contains("not awaited"));
    assert!(text.ends_with("ValueError: example"));
}

#[test]
fn failure_warnings_distinguish_preflight_and_script_without_host_calls() {
    let mut text = String::new();
    append_error_output(
        &mut text,
        "Cannot initialize interpreter",
        true,
        &ExecutionProgress::default(),
        MAX_OUTPUT_CHARS,
    );
    assert!(text.contains("Code did not run"));
    assert!(text.contains("REPL state is unchanged"));
    assert!(!text.contains("external tool effects"));

    let mut text = String::new();
    append_error_output(
        &mut text,
        "MemoryError",
        true,
        &ExecutionProgress {
            script_started: true,
            host_call_started: AtomicBool::new(false),
        },
        MAX_OUTPUT_CHARS,
    );
    assert!(text.contains("changes from this call were discarded"));
    assert!(text.contains("REPL state is unchanged"));
    assert!(!text.contains("external tool effects"));
}

#[test]
fn checkpoint_limit_reports_actual_size_and_separate_memory_budget() {
    assert!(validate_checkpoint_size(MAX_CODEMODE_SNAPSHOT_BYTES).is_ok());
    let size = MAX_CODEMODE_SNAPSHOT_BYTES + 1;
    let error = validate_checkpoint_size(size).unwrap_err();
    assert!(error.contains(&format!("{size} bytes")), "{error}");
    assert!(error.contains("16 MiB"), "{error}");
    assert!(error.contains("64 MiB"), "{error}");
    assert!(error.contains("write()"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn sleep_waits_until_deadline_and_can_be_cancelled() {
    let (abort, aborted) = watch::channel(false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let start = tokio::time::Instant::now();
    let mut sleep = Box::pin(wait_for_sleep(deadline, aborted.clone()));
    assert!(
        tokio::time::timeout(Duration::from_secs(9), &mut sleep)
            .await
            .is_err()
    );
    assert!(matches!(sleep.await, ResumeValue::Return(_)));
    assert_eq!(start.elapsed(), Duration::from_secs(10));

    abort.send(true).unwrap();
    let sleep = wait_for_sleep(
        tokio::time::Instant::now() + Duration::from_secs(3600),
        aborted,
    );
    assert!(matches!(sleep.await, ResumeValue::Error(_)));
    assert_eq!(start.elapsed(), Duration::from_secs(10));
}

#[tokio::test]
async fn dropping_bundle_invalidates_retained_handles() {
    let bundle = super::super::CodeModeBundle::new();
    let engine = bundle.engine.clone();
    let mut active = engine.shutdown.subscribe();
    drop(bundle);
    active.changed().await.unwrap();
    assert!(*active.borrow());
    assert!(*engine.shutdown.subscribe().borrow());
    engine.close().await;
    engine.close().await;
}

#[test]
fn arguments_accept_object_or_keywords_and_reject_non_json_values() {
    let mut args = CallArgs::new();
    args.push_kwarg("name", MontyObject::string("hello"));
    args.push_kwarg(
        "flags",
        MontyObject::list([MontyObject::bool(true), MontyObject::none()]),
    );
    assert_eq!(
        parse_call_arguments(&args).unwrap(),
        json!({"name": "hello", "flags": [true, null]})
    );

    let mut args = CallArgs::new();
    args.push_arg(MontyObject::dict([(
        MontyObject::string("n"),
        MontyObject::int(7),
    )]));
    assert_eq!(parse_call_arguments(&args).unwrap(), json!({"n": 7}));

    let mut args = CallArgs::new();
    args.push_arg(MontyObject::int(7));
    assert_eq!(
        parse_call_arguments(&args).unwrap_err(),
        "Expected an argument object"
    );

    let mut args = CallArgs::new();
    args.push_arg(MontyObject::dict([]));
    args.push_kwarg("name", MontyObject::string("hello"));
    assert_eq!(
        parse_call_arguments(&args).unwrap_err(),
        "Use keyword arguments or one argument object"
    );

    let mut args = CallArgs::new();
    args.push_kwarg("bad", MontyObject::bytes([0, 1]));
    assert!(parse_call_arguments(&args).unwrap_err().contains("bytes"));
}

#[test]
fn prior_results_accepts_positional_indices_without_changing_tool_argument_rules() {
    let prior = CodeModeFunctionKind::PriorResults(
        crate::components::codemode_functions::prior_results::PriorResults,
    );
    let mut args = CallArgs::new();
    args.push_arg(MontyObject::int(-2));
    assert_eq!(
        parse_function_arguments(&prior, &args).unwrap(),
        json!({"index": -2})
    );

    let tool = CodeModeFunctionKind::Tool(crate::components::bundles::testing::test_tool(
        "noop",
        psa::api::ToolId::Edit,
    ));
    assert!(parse_function_arguments(&tool, &args).is_err());

    let mut args = CallArgs::new();
    args.push_kwarg("index", MontyObject::int(0));
    assert_eq!(
        parse_function_arguments(&prior, &args).unwrap(),
        json!({"index": 0})
    );
    assert_eq!(
        parse_function_arguments(&prior, &CallArgs::new()).unwrap(),
        json!({})
    );
}

#[tokio::test]
async fn snapshot_validation_rejects_suspensions_but_accepts_python_exceptions() {
    let Some(worker) = std::env::var_os("PSA_MONTY_WORKER") else {
        eprintln!("skipping Monty worker test: PSA_MONTY_WORKER is not set");
        return;
    };
    let mut config = PoolConfig::subprocess(worker);
    config.min_processes = 0;
    config.max_processes = 1;
    config.request_timeout = Some(Duration::from_secs(5));
    let pool = Pool::new(config).await.unwrap();
    let repl = ReplConfig::default();
    let mut session = pool.checkout(&repl).await.unwrap();
    let mut print = on_print_sync(|_, _: &str| {});

    let error = session
        .feed(
            "x = 4\nraise ValueError('oops')",
            vec![],
            vec![],
            false,
            &mut print,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, PoolError::Runtime(_)));
    let snapshot = session.dump().await.unwrap();
    session.finish().await.unwrap();
    validate_idle_snapshot(&pool, &repl, &snapshot)
        .await
        .unwrap();

    let mut session = pool.checkout(&repl).await.unwrap();
    assert!(matches!(
        session
            .feed("host_call()", vec![], vec![], false, &mut print)
            .await
            .unwrap(),
        TurnEvent::FunctionCall { .. }
    ));
    let snapshot = session.dump().await.unwrap();
    session.finish().await.unwrap();
    assert!(
        validate_idle_snapshot(&pool, &repl, &snapshot)
            .await
            .unwrap_err()
            .contains("suspended")
    );
    pool.close().await;
}

#[test]
fn json_round_trip_and_integer_overflow() {
    let value = json!({"a": [1, false, null, "é"], "nested": {"x": -3}});
    let object = json_to_monty(value.clone()).unwrap();
    let mut budget = 1000;
    assert_eq!(
        monty_to_json(object.as_ref(), 0, &mut budget).unwrap(),
        value
    );
    assert!(json_to_monty(json!(u64::MAX)).is_err());
}

#[test]
fn model_view_is_bounded_while_saved_output_keeps_everything() {
    let full: String = (0..5_000).map(|line| format!("line {line}\n")).collect();
    let view = bound_output(&full, MAX_PRINT_CHARS);
    assert!(view.contains("[output truncated]"));
    assert!(view.chars().count() <= MAX_PRINT_CHARS);
    assert!(view.starts_with("line 0\n"));
    assert!(view.ends_with("line 4999\n"));
    assert!(!view.contains("line 2500\n"));

    let mut saved = BoundedOutput::new(MAX_SAVED_OUTPUT_CHARS);
    saved.push_str(&full);
    assert_eq!(saved.finish(), full);
}
