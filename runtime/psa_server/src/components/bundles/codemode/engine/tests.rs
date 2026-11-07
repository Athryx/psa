use super::*;
use serde_json::json;

#[test]
fn wall_deadline_covers_maximum_registered_wait() {
    assert!(WALL_TIMEOUT > Duration::from_secs(600));
    assert_eq!(FEED_TIMEOUT, Duration::from_secs(10));
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
    );
    assert!(text.contains("[output truncated]"));
    assert!(text.contains("ValueError:"));
    assert!(text.contains("REPL changes were not saved; external tool effects may have occurred."));
    assert!(text.chars().count() <= MAX_OUTPUT_CHARS);

    let mut text = String::new();
    append_error_output(&mut text, "ValueError: failure", false);
    assert_eq!(text, "ValueError: failure");
}

#[test]
fn unresolved_calls_keep_exception_and_warn_about_unsaved_state() {
    let error = unresolved_calls_error(Some("ValueError: example"));
    let mut text = String::new();
    append_error_output(&mut text, &error, true);
    assert!(text.contains("REPL changes were not saved"));
    assert!(text.contains("external tool effects may have occurred"));
    assert!(text.ends_with("Execution ended with unresolved host calls\nValueError: example"));
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
