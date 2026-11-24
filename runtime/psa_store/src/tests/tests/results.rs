use super::*;
use crate::{EventRange, NewEvent};
use psa::api::AgentResult;

fn completion(value: AgentResult) -> [NewEvent; 3] {
    [
        state_change(AgentState::Running),
        NewEvent::new(EventType::Result(value)),
        state_change(AgentState::Idle),
    ]
}

#[tokio::test]
async fn result_history_is_newest_first_durable_and_bounded_by_every_fork() {
    let (directory, db, session, parent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    assert!(
        cursor
            .load_last_result(session.id, parent.id)
            .await
            .unwrap()
            .is_none()
    );
    let first = cursor
        .add_events(
            parent.id,
            &[
                state_change(AgentState::Running),
                log("not a result"),
                NewEvent::new(EventType::Result(AgentResult::Text("first".into()))),
                state_change(AgentState::Idle),
            ],
        )
        .await
        .unwrap();
    let first_id = first.events[2].id;
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: parent.id,
                    event_id: first_id,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0];
    assert_eq!(
        child.state,
        AgentState::Idle,
        "a Result fork point is logically terminal before its paired Idle event"
    );
    cursor
        .add_events(
            parent.id,
            &completion(AgentResult::Json(
                serde_json::json!({"private_parent":true}),
            )),
        )
        .await
        .unwrap();
    let child_saved = cursor
        .add_events(child.id, &completion(AgentResult::Text("child".into())))
        .await
        .unwrap();
    let child_result_id = child_saved.events[1].id;
    let nested = cursor
        .fork_session(
            fork.session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: child.id,
                    event_id: child_result_id,
                },
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let descendant = &nested.agents[0];
    assert_eq!(descendant.state, AgentState::Idle);
    cursor
        .add_events(
            child.id,
            &completion(AgentResult::Text("private child continuation".into())),
        )
        .await
        .unwrap();
    cursor
        .add_events(
            descendant.id,
            &completion(AgentResult::Json(serde_json::json!({"answer":42}))),
        )
        .await
        .unwrap();

    let results = cursor
        .load_last_results(nested.session.id, descendant.id, 10)
        .await
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|record| record.value.clone())
            .collect::<Vec<_>>(),
        vec![
            AgentResult::Json(serde_json::json!({"answer":42})),
            AgentResult::Text("child".into()),
            AgentResult::Text("first".into()),
        ]
    );
    assert_eq!(
        results
            .iter()
            .map(|record| record.source_agent_id)
            .collect::<Vec<_>>(),
        vec![descendant.id, child.id, parent.id]
    );
    assert!(
        results.iter().all(
            |record| record.session_id == nested.session.id && record.agent_id == descendant.id
        )
    );
    assert!(
        results
            .windows(2)
            .all(|pair| pair[0].event_id > pair[1].event_id)
    );
    assert_eq!(
        cursor
            .load_last_result(nested.session.id, descendant.id)
            .await
            .unwrap(),
        Some(results[0].clone())
    );
    assert_eq!(
        cursor
            .load_last_results(nested.session.id, descendant.id, 2)
            .await
            .unwrap(),
        results[..2]
    );
    for limit in [0, 10001] {
        assert!(
            cursor
                .load_last_results(nested.session.id, descendant.id, limit)
                .await
                .is_err()
        );
    }
    assert!(
        cursor
            .load_last_result(session.id, descendant.id)
            .await
            .is_err()
    );

    // Limits apply after filtering, and pagination cursors remain full transcript IDs.
    let mut after = EventId(0);
    let mut filtered = Vec::new();
    loop {
        let page = cursor
            .load_agent_result_envelopes(
                nested.session.id,
                descendant.id,
                EventRange {
                    after,
                    limit: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        assert!(matches!(page[0].event.kind, EventType::Result(_)));
        after = page[0].event.id;
        filtered.push(after);
    }
    assert_eq!(
        filtered,
        results
            .iter()
            .rev()
            .map(|record| record.event_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(filtered[0], first_id);
    let bounded = cursor
        .load_agent_result_envelopes(
            nested.session.id,
            descendant.id,
            EventRange {
                through: Some(child_result_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(bounded.len(), 2);
    let session_results = cursor
        .load_session_result_events(nested.session.id, 0, None, 10)
        .await
        .unwrap();
    assert_eq!(session_results.len(), 3);
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.read().await.unwrap();
    assert_eq!(
        cursor
            .load_last_results(nested.session.id, descendant.id, 10)
            .await
            .unwrap(),
        results
    );
    assert_eq!(
        cursor.get_agent(descendant.id).await.unwrap().state,
        AgentState::Idle
    );
    cursor.close().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn results_require_running_and_an_immediate_idle_transition_in_one_batch() {
    let (_directory, db, session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    for invalid in [
        vec![
            NewEvent::new(EventType::Result(AgentResult::Text("not running".into()))),
            state_change(AgentState::Idle),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("missing idle".into()))),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("interleaved".into()))),
            log("between result and idle"),
            state_change(AgentState::Idle),
        ],
        vec![
            state_change(AgentState::Running),
            NewEvent::new(EventType::Result(AgentResult::Text("first".into()))),
            state_change(AgentState::Idle),
            NewEvent::new(EventType::Result(AgentResult::Text("duplicate".into()))),
            state_change(AgentState::Idle),
        ],
    ] {
        assert!(cursor.add_events(agent.id, &invalid).await.is_err());
        assert!(cursor.load_all_events(agent.id).await.unwrap().is_empty());
        assert_eq!(
            cursor.get_agent(agent.id).await.unwrap().state,
            AgentState::Idle
        );
        assert!(
            cursor
                .load_last_result(session.id, agent.id)
                .await
                .unwrap()
                .is_none()
        );
    }
    cursor
        .add_events(
            agent.id,
            &completion(AgentResult::Json(serde_json::json!({"valid":true}))),
        )
        .await
        .unwrap();
    assert_eq!(
        cursor
            .load_last_results(session.id, agent.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}
