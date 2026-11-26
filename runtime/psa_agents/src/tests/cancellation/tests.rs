use super::*;
use crate::agent_manager::AgentClient;
use psa_store::SessionId;
use std::time::Duration;

fn session() -> Session {
    Session {
        decision_model: None,
        id: SessionId(1),
        probe_id: psa::api::ProbeId(psa::api::probes::Uuid::nil()),
        forked_from: None,
        first_position: 1,
        next_position: 1,
        name: "test".into(),
        working_directory: "/work".into(),
        project_id: None,
        model_config: crate::test_support::model_config(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[tokio::test]
async fn cancellation_awaits_cooperative_success_and_bounds_uncooperative_cleanup() {
    let mut session = session();
    let (controls, mut received) = actors::unbounded_channel();
    let controls = AgentClient::new(controls);
    controls.interrupt().unwrap();
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

    controls.interrupt().unwrap();
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
