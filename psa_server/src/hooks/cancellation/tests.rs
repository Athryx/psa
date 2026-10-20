use super::*;
use crate::sessions::SessionId;
use std::time::Duration;

fn session() -> Session {
    Session {
        id: SessionId(1),
        parent_id: None,
        first_position: 1,
        next_position: 1,
        name: "test".into(),
        working_directory: "/work".into(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[tokio::test]
async fn cancellation_awaits_cooperative_success_and_bounds_uncooperative_cleanup() {
    let mut session = session();
    let (controls, mut received) = mpsc::unbounded_channel();
    controls.send(AgentManagerMessage::Interrupt).unwrap();
    let (sender, mut cancellation) = Cancellation::channel();
    let (result, interrupted) = run_cancellable(
        async {
            cancellation.cancelled().await;
            42
        },
        sender,
        Duration::from_secs(1),
        &mut session,
        &mut received,
    )
    .await;
    assert!(interrupted);
    assert_eq!(result, Some(42));

    controls.send(AgentManagerMessage::Interrupt).unwrap();
    let (sender, mut cancellation) = Cancellation::channel();
    let (result, interrupted) = run_cancellable(
        async {
            cancellation.cancelled().await;
            std::future::pending::<()>().await
        },
        sender,
        Duration::from_millis(1),
        &mut session,
        &mut received,
    )
    .await;
    assert!(interrupted);
    assert!(result.is_none());
}

#[tokio::test]
async fn cancellation_is_latched_and_wait_is_cancel_safe() {
    let (sender, mut cancellation) = Cancellation::channel();
    assert!(!cancellation.is_cancelled());
    tokio::select! { biased; _ = std::future::ready(()) => {}, _ = cancellation.cancelled() => panic!() }
    sender.send(()).unwrap();
    cancellation.cancelled().await;
    cancellation.cancelled().await;
    assert!(cancellation.is_cancelled());
    let (sender, mut cancellation) = Cancellation::channel();
    drop(sender);
    cancellation.cancelled().await;
    assert!(cancellation.is_cancelled());
}
