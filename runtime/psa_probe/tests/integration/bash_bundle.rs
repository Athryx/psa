use super::*;
use psa_server::{
    agents::NewAgent,
    components::{
        bundles::{
            BashBundle, Bundle, Installer,
            bash::{BashError, BashHandle},
        },
        cancellation::Cancellation,
    },
    sessions::NewSession,
};

pub(super) async fn install_bundle(harness: &Harness) -> (BashBundle, BashHandle) {
    let database = Database::open(harness.dir.path().join("server.sqlite3"))
        .await
        .unwrap();
    let mut cursor = database.cursor().await.unwrap();
    let definition = cursor
        .create_agent_definition(psa_server::db::CreateAgentDefinition {
            name: "bash integration".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            name: "bash integration".into(),
            working_directory: harness.dir.path().into(),
            probe_id: harness.id,
            project_id: None,
            model_config: serde_json::from_value(serde_json::json!({
                "model":"openrouter/test-model","effort":"Medium"
            }))
            .unwrap(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                definition_id: definition.id,
                name: "bash integration".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let mut bundle = BashBundle::new();
    let mut installer = Installer::default();
    bundle.install(&agent, &session, &mut installer).unwrap();
    let handle = bundle.handle();
    drop(installer);
    database.close().await;
    (bundle, handle)
}

pub(super) fn cancellation(senders: &mut Vec<oneshot::Sender<()>>) -> Cancellation {
    let (sender, cancellation) = Cancellation::channel();
    senders.push(sender);
    cancellation
}

pub(super) async fn remove_logs(probe: &ProbeHandle, paths: &[std::path::PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let mut command = Command::new("sh", "/").args(["-c", "rm -f -- \"$@\"", "bash-test-cleanup"]);
    command
        .args
        .extend(paths.iter().map(|path| path.to_str().unwrap().to_owned()));
    let (_, _, status) = collect(probe.spawn(command).await.unwrap()).await;
    assert_eq!(status.unwrap().code, Some(0));
}

#[tokio::test]
async fn bash_bundle_handles_background_stdin_intervals_duplicates_and_close() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let mut cancellations = Vec::new();
        let harness = Harness::start().await;
        let probe = harness.handle().await;
        let (mut bundle, bash) = install_bundle(&harness).await;
        let cwd = harness.dir.path().to_owned();
        let missing = cwd.join("does-not-exist");
        let error = bash.execute(probe.clone(), Command::new("bash", &missing).args(["-c", "true"]),
            Some("missing-cwd".into()), Duration::ZERO, cancellation(&mut cancellations)).await;
        assert!(matches!(error, Err(BashError::Other(message)) if message == format!(
            "Working directory does not exist: {}\nCannot execute bash commands.", missing.display())));

        let initial = bash.execute(probe.clone(), Command::new("bash", &cwd)
            .args(["-c", "printf 'first\\n'; IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 0.1; printf done"]),
            Some("interactive".into()), Duration::from_millis(30), cancellation(&mut cancellations)).await.unwrap();
        assert!(initial.running);
        let log = initial.log_path.clone().expect("background session keeps its log");
        let mut combined = initial.output_text().to_owned();
        assert!(matches!(bash.execute(probe.clone(), Command::new("bash", &cwd).args(["-c", "true"]),
            Some("interactive".into()), Duration::ZERO, cancellation(&mut cancellations)).await,
            Err(BashError::DuplicateName(name)) if name == "interactive"));

        let polled = bash.interact("interactive".into(), None, Duration::from_millis(30), cancellation(&mut cancellations)).await.unwrap();
        combined.push_str(polled.output_text());
        assert!(polled.running);
        let completed = bash.interact("interactive".into(), Some("hello\n".into()), Duration::from_secs(3), cancellation(&mut cancellations)).await.unwrap();
        combined.push_str(completed.output_text());
        assert!(!completed.running);
        assert_eq!(completed.exit_code, Some(0));
        assert_eq!(combined.matches("first\n").count(), 1);
        assert!(combined.contains("got:hello\n"));
        assert!(combined.ends_with("done"));
        let finished = bash.interact("interactive".into(), None, Duration::ZERO, cancellation(&mut cancellations)).await.unwrap();
        assert!(finished.already_finished && !finished.running);
        assert_eq!(finished.exit_code, Some(0));
        assert!(finished.output_text().is_empty());
        assert!(matches!(bash.interact("interactive".into(), Some("more\n".into()), Duration::ZERO, cancellation(&mut cancellations)).await,
            Err(BashError::StdinClosed(_))));
        let (full, _, _) = collect(probe.read_file(&log).await.unwrap()).await;
        assert_eq!(full, combined.as_bytes());

        let running = bash.execute(probe.clone(), Command::new("bash", &cwd).args(["-c", "sleep 30"]),
            Some("close-me".into()), Duration::ZERO, cancellation(&mut cancellations)).await.unwrap();
        assert!(running.running);
        let closed = bash.close("close-me".into()).await.unwrap();
        assert!(!closed.running);
        assert!(bash.close("close-me".into()).await.unwrap().already_finished);
        assert!(matches!(bash.close("never-started".into()).await, Err(BashError::UnknownSession(_))));

        bundle.cleanup().await.unwrap();
        let closed_log = closed.log_path.expect("closed session keeps its log");
        remove_logs(&probe, &[log, closed_log]).await;
        harness.stop().await;
    }).await.unwrap();
}

#[tokio::test]
async fn bash_bundle_retains_full_output_and_cleans_up_live_processes() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let mut cancellations = Vec::new();
        let harness = Harness::start().await;
        let probe = harness.handle().await;
        let (mut bundle, bash) = install_bundle(&harness).await;
        let cwd = harness.dir.path().to_owned();
        let output = bash.execute(probe.clone(), Command::new("bash", &cwd).args(["-c",
            "i=0; while [ \"$i\" -lt 3000 ]; do printf 'line%04d\\n' \"$i\"; i=$((i+1)); done"]),
            None, Duration::from_secs(10), cancellation(&mut cancellations)).await.unwrap();
        assert!(!output.running);
        assert!(output.was_truncated());
        assert!(output.output_text().starts_with("line0000\n"));
        // The extension's truncated preview joins retained lines without the
        // final newline; the full log still preserves the original bytes.
        assert!(output.output_text().ends_with("line2999"));
        assert!(output.output_text().contains("[middle output omitted]"));
        assert!(output.output_text().len() <= 50 * 1024);
        let log = output.log_path.expect("truncated output keeps its log");
        let (full, _, _) = collect(probe.read_file(&log).await.unwrap()).await;
        let full = String::from_utf8(full).unwrap();
        assert_eq!(full.lines().count(), 3000);
        assert!(full.ends_with("line2999\n"));

        let pid_path = cwd.join("bash-bundle.pid");
        let live = bash
            .execute(
                probe.clone(),
                Command::new("bash", &cwd).args([
                    "-c",
                    "printf '%s' \"$$\" > \"$1\"; printf ready; sleep 30",
                    "bash-test",
                    pid_path.to_str().unwrap(),
                ]),
                Some("shutdown".into()),
                Duration::from_millis(50),
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(live.running);
        // A short collection window does not guarantee that the remote shell
        // has written its PID yet; wait for its explicit readiness signal.
        let mut readiness = live.output_text().to_owned();
        while !readiness.contains("ready") {
            let interval = bash
                .interact(
                    "shutdown".into(),
                    None,
                    Duration::from_millis(100),
                    cancellation(&mut cancellations),
                )
                .await
                .unwrap();
            assert!(interval.running, "process exited before readiness");
            readiness.push_str(interval.output_text());
        }
        let (pid, _, _) = collect(probe.read_file(&pid_path).await.unwrap()).await;
        let pid = String::from_utf8(pid).unwrap();
        bundle.cleanup().await.unwrap();
        bundle.cleanup().await.unwrap();
        let (_, _, status) = collect(
            probe
                .spawn(Command::new("sh", &cwd).args([
                    "-c",
                    "kill -0 \"$1\" 2>/dev/null",
                    "bash-test",
                    &pid,
                ]))
                .await
                .unwrap(),
        )
        .await;
        assert_ne!(status.unwrap().code, Some(0));
        let live_log = live.log_path.expect("background session keeps its log");
        remove_logs(&probe, &[log, live_log]).await;
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bash_initial_cancellation_removes_the_named_session() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut cancellations = Vec::new();
        let harness = Harness::start().await;
        let probe = harness.handle().await;
        let (mut bundle, bash) = install_bundle(&harness).await;
        let (sender, cancellation) = Cancellation::channel();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = sender.send(());
        });
        let result = bash
            .execute(
                probe,
                Command::new("bash", harness.dir.path()).args(["-c", "sleep 30"]),
                Some("cancelled".into()),
                Duration::from_secs(10),
                cancellation,
            )
            .await;
        cancel.await.unwrap();
        assert!(matches!(result, Err(BashError::Interrupted)));
        assert!(matches!(
            bash.interact(
                "cancelled".into(),
                None,
                Duration::ZERO,
                self::cancellation(&mut cancellations)
            )
            .await,
            Err(BashError::UnknownSession(_))
        ));
        bundle.cleanup().await.unwrap();
        harness.stop().await;
    })
    .await
    .unwrap();
}
