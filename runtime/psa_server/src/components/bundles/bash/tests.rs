use super::state::{Names, Runtime};
use super::*;

#[test]
fn names_are_reserved_and_automatic_names_skip_explicit_names() {
    let mut names = Names::default();
    assert_eq!(
        names.reserve(Some("bash1".into()), |_| false).unwrap(),
        "bash1"
    );
    assert_eq!(
        names.reserve(None, |name| name == "bash1").unwrap(),
        "bash2"
    );
    assert_eq!(names.reserve(None, |_| false).unwrap(), "bash3");
    for whitespace in [" \t", "\u{feff}", "\u{2028}\u{3000}"] {
        assert!(matches!(
            names.reserve(Some(whitespace.into()), |_| false),
            Err(BashError::InvalidName)
        ));
    }
    assert_eq!(
        names.reserve(Some("\u{0085}".into()), |_| false).unwrap(),
        "\u{0085}"
    );
    assert!(
        matches!(names.reserve(Some("existing".into()), |_| true), Err(BashError::DuplicateName(name)) if name == "existing")
    );
}

#[test]
fn construction_does_not_need_a_tokio_runtime() {
    let bundle = BashBundle::new();
    let _handle = bundle.handle();
}

#[tokio::test]
async fn cleanup_is_idempotent_and_invalidates_handles() {
    let mut runtime = Runtime::new();
    let handle = runtime.handle();
    assert!(
        matches!(handle.close("missing".into()).await, Err(BashError::UnknownSession(name)) if name == "missing")
    );
    runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        handle.close("missing".into()).await,
        Err(BashError::Unavailable)
    ));
}

#[tokio::test]
async fn dropping_owner_invalidates_even_retained_tool_handles() {
    let runtime = Runtime::new();
    let handle = runtime.handle();
    drop(runtime);
    assert!(matches!(
        handle.close("missing".into()).await,
        Err(BashError::Unavailable)
    ));
}

#[test]
fn remote_shell_resolution_preserves_protocol_environment_and_stdin() {
    use psa_api::probes::{Command, Stdio};
    let command = Command::new("bash", "/work dir")
        .args(["-c", "printf '%s' '$injection'"])
        .env("EXAMPLE_SET", "id")
        .env_remove("EXAMPLE_REMOVED");
    let command = log::resolve_shell(command);
    assert_eq!(command.program, "sh");
    assert_eq!(command.cwd, PathBuf::from("/work dir"));
    assert_eq!(command.args.last().unwrap(), "printf '%s' '$injection'");
    assert_eq!(command.env.get("EXAMPLE_SET"), Some(&Some("id".into())));
    assert_eq!(command.env.get("EXAMPLE_REMOVED"), Some(&None));
    assert!(!command.env_clear);
    assert!(matches!(command.stdin, Stdio::Piped));
}

#[tokio::test]
async fn abort_task_does_not_detach_owned_work() {
    let (sender, received) = tokio::sync::oneshot::channel::<()>();
    let task = state::AbortTask(tokio::spawn(async move {
        let _sender = sender;
        std::future::pending::<()>().await;
    }));
    drop(task);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), received)
            .await
            .unwrap()
            .is_err()
    );
}
