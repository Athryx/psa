use super::{
    bash_bundle::{cancellation, install_bundle, remove_logs},
    *,
};
use psa_server::components::{
    bundles::{Bundle, bash::BashError},
    cancellation::Cancellation,
};

#[tokio::test]
async fn bash_poll_cancellation_preserves_unread_output_and_close_rejects_busy_session() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let harness = Harness::start().await;
        let probe = harness.handle().await;
        let (mut bundle, bash) = install_bundle(&harness).await;
        let mut cancellations = Vec::new();
        let initial = bash
            .execute(
                probe.clone(),
                Command::new("bash", harness.dir.path()).args([
                    "-c",
                    "sleep 0.2; printf later; IFS= read -r line; printf end",
                ]),
                Some("keep-unread".into()),
                Duration::ZERO,
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(initial.running);

        let (cancel, token) = Cancellation::channel();
        let poll = bash.interact("keep-unread".into(), None, Duration::from_secs(5), token);
        tokio::pin!(poll);
        tokio::select! {
            biased;
            result = &mut poll => panic!("poll unexpectedly completed: {}", result.is_ok()),
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
        assert!(matches!(
            bash.close("keep-unread".into()).await,
            Err(BashError::Busy(_))
        ));
        cancel.send(()).unwrap();
        assert!(matches!(poll.await, Err(BashError::Interrupted)));

        let unread = bash
            .interact(
                "keep-unread".into(),
                None,
                Duration::from_millis(500),
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(unread.running);
        assert_eq!(
            format!("{}{}", initial.output_text(), unread.output_text()),
            "later"
        );
        let empty = bash
            .interact(
                "keep-unread".into(),
                None,
                Duration::ZERO,
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(empty.output_text().is_empty());
        let done = bash
            .interact(
                "keep-unread".into(),
                Some("go\n".into()),
                Duration::from_secs(3),
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(!done.running);
        assert_eq!(done.output_text(), "end");
        let log = initial.log_path.expect("background session keeps its log");
        let (full, _, _) = collect(probe.read_file(&log).await.unwrap()).await;
        assert_eq!(full, b"laterend");

        bundle.cleanup().await.unwrap();
        remove_logs(&probe, &[log]).await;
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bash_foreground_success_deletes_log_but_failure_retains_it() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let probe = harness.handle().await;
        let (mut bundle, bash) = install_bundle(&harness).await;
        let mut cancellations = Vec::new();
        let success = bash
            .execute(
                probe.clone(),
                Command::new("bash", harness.dir.path()).args(["-c", "true"]),
                None,
                Duration::from_secs(3),
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(!success.running);
        assert_eq!(success.exit_code, Some(0));
        assert!(success.log_path.is_none());

        let failed = bash
            .execute(
                probe.clone(),
                Command::new("bash", harness.dir.path()).args(["-c", "printf failure; exit 7"]),
                None,
                Duration::from_secs(3),
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(!failed.running);
        assert_eq!(failed.exit_code, Some(7));
        assert_eq!(failed.output_text(), "failure");
        let failed_log = failed
            .log_path
            .clone()
            .expect("failed command keeps its log");
        let (full, _, _) = collect(probe.read_file(&failed_log).await.unwrap()).await;
        assert_eq!(full, b"failure");
        let finished = bash
            .interact(
                failed.name,
                None,
                Duration::ZERO,
                cancellation(&mut cancellations),
            )
            .await
            .unwrap();
        assert!(finished.already_finished);
        assert_eq!(finished.exit_code, Some(7));
        assert_eq!(finished.log_path, Some(failed_log.clone()));

        bundle.cleanup().await.unwrap();
        remove_logs(&probe, &[failed_log]).await;
        harness.stop().await;
    })
    .await
    .unwrap();
}
