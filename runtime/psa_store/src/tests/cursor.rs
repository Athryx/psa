use super::{fixture, log, state_change};
use crate::{DbError, ForkPoint, NewAgent, NewSession, SessionFork, SessionUpdate};
use crate::{EventRange, NewEvent};
use llm_api::api::{Outcome, Usage};
use psa::api::AgentState;
use psa::api::events::*;
use std::time::Duration;

#[tokio::test]
async fn commit_persists_multiple_operations_together() {
    let (_dir, db, _session, _agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            model_config: crate::test_support::model_config(),
            probe_id: psa::api::ProbeId(psa::api::probes::Uuid::nil()),
            project_id: None,
            name: "atomic setup".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa::api::AgentDefinitionId(1),
                name: "main".into(),
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(agent.id, &[state_change(AgentState::Running), log("ready")])
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );

    // Releasing operation savepoints must not expose any writes before commit.
    let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(session_count, 1);
    cursor.commit().await.unwrap();

    let mut cursor = db.write().await.unwrap();
    assert_eq!(
        cursor.get_session(session.id).await.unwrap().name,
        "atomic setup"
    );
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Running
    );
    let child = &fork.agents[0];
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().state,
        AgentState::Running
    );
    assert_eq!(
        cursor
            .load_events(child.id, EventRange::default())
            .await
            .unwrap()
            .len(),
        2
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn explicit_rollback_and_drop_discard_all_cursor_changes() {
    let (_dir, db, session, agent) = fixture().await;
    for explicit in [true, false] {
        let mut cursor = db.write().await.unwrap();
        cursor
            .update_session(
                session.id,
                SessionUpdate {
                    name: Some("uncommitted".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        cursor
            .add_events(
                agent.id,
                &[state_change(AgentState::Running), log("uncommitted")],
            )
            .await
            .unwrap();
        cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    definition_id: psa::api::AgentDefinitionId(1),
                    name: "uncommitted".into(),
                },
            )
            .await
            .unwrap();
        cursor
            .fork_session(
                session.id,
                SessionFork {
                    point: ForkPoint::Head,
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        if explicit {
            cursor.rollback().await.unwrap();
        } else {
            drop(cursor);
        }

        let mut cursor = db.write().await.unwrap();
        let unchanged = cursor.get_session(session.id).await.unwrap();
        assert_eq!(unchanged.name, session.name);
        assert_eq!(unchanged.next_position, session.next_position);
        let unchanged = cursor.get_agent(agent.id).await.unwrap();
        assert_eq!(unchanged.state, AgentState::Idle);
        assert_eq!(unchanged.next_event_id, agent.next_event_id);
        assert_eq!(cursor.list_agents(session.id).await.unwrap().len(), 1);
        assert!(
            cursor
                .list_sessions(Some(session.id))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            cursor
                .load_events(agent.id, EventRange::default())
                .await
                .unwrap()
                .is_empty()
        );
        cursor.rollback().await.unwrap();
    }
    db.close().await;
}

#[tokio::test]
async fn failed_operation_does_not_discard_earlier_cursor_writes() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    cursor
        .update_session(
            session.id,
            SessionUpdate {
                name: Some("keep this".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    cursor
        .add_events(agent.id, &[state_change(AgentState::Running)])
        .await
        .unwrap();
    let bad = NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
        id: 1,
        outcome: Outcome::InProgress,
        usage: Usage::default(),
        request_time: Duration::ZERO,
        response_id: None,
        provider_metadata: Default::default(),
    }));
    assert!(matches!(
        cursor
            .add_events(agent.id, &[state_change(AgentState::Idle), bad])
            .await,
        Err(DbError::InvalidInput(_))
    ));
    cursor.commit().await.unwrap();

    let mut cursor = db.read().await.unwrap();
    assert_eq!(
        cursor.get_session(session.id).await.unwrap().name,
        "keep this"
    );
    let saved = cursor.get_agent(agent.id).await.unwrap();
    assert_eq!(saved.state, AgentState::Running);
    assert_eq!(saved.next_event_id, EventId(2));
    assert_eq!(
        cursor
            .load_events(agent.id, EventRange::default())
            .await
            .unwrap()
            .len(),
        1
    );
    cursor.close().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn cancelling_a_task_rolls_back_its_cursor() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_dir, db, session, agent) = fixture().await;
        let writer = db.clone();
        let (ready, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut cursor = writer.write().await.unwrap();
            cursor
                .add_events(agent.id, &[state_change(AgentState::Running)])
                .await
                .unwrap();
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        wait.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let mut cursor = db.write().await.unwrap();
        assert_eq!(
            cursor.get_agent(agent.id).await.unwrap().state,
            AgentState::Idle
        );
        assert_eq!(
            cursor.get_session(session.id).await.unwrap().next_position,
            session.next_position
        );
        assert!(
            cursor
                .load_events(agent.id, EventRange::default())
                .await
                .unwrap()
                .is_empty()
        );
        cursor.rollback().await.unwrap();
        db.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn read_cursors_do_not_wait_for_an_open_writer() {
    tokio::time::timeout(Duration::from_secs(2), async {
        let (_dir, db, _session, agent) = fixture().await;
        let mut writer = db.write().await.unwrap();
        writer
            .add_events(agent.id, &[state_change(AgentState::Running)])
            .await
            .unwrap();

        // A second writer would wait out the busy timeout; readers see the
        // last committed snapshot immediately.
        let mut reader = db.read().await.unwrap();
        assert_eq!(
            reader.get_agent(agent.id).await.unwrap().state,
            AgentState::Idle
        );
        reader.close().await.unwrap();

        writer.commit().await.unwrap();
        let mut reader = db.read().await.unwrap();
        assert_eq!(
            reader.get_agent(agent.id).await.unwrap().state,
            AgentState::Running
        );
        reader.close().await.unwrap();
        db.close().await;
    })
    .await
    .unwrap();
}
