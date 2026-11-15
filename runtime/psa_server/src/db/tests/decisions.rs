use super::fixture;
use crate::sessions::{ForkPoint, SessionFork, SessionUpdate};
use llm_api::providers::{Model, ProviderType};
use psa_api::{
    DecisionAnswerResult, DecisionAnswerSummary, DecisionCall, DecisionCallUsage, DecisionOutcome,
    DecisionQuestionSummary,
    events::{EventType, NewEvent},
};
use std::time::Duration;

fn decider(name: &str) -> Model {
    Model::new(ProviderType::Openrouter, name).unwrap()
}

#[tokio::test]
async fn decision_call_events_round_trip_through_the_database() {
    let (_dir, db, _session, agent) = fixture().await;
    let call = DecisionCall {
        tool_call_id: Some("call-1".into()),
        model: decider("vendor/decider"),
        questions: vec![DecisionQuestionSummary {
            key: "angry".into(),
            kind: "noul".into(),
        }],
        outcome: DecisionOutcome::PartlyAnswered,
        answers: vec![DecisionAnswerSummary {
            key: "angry".into(),
            result: DecisionAnswerResult::Noul(0.25),
            confidence: Some(0.9),
        }],
        usage: DecisionCallUsage {
            input_tokens: Some(128),
            cached_tokens: Some(64),
            cost: Some(0.0000384),
        },
        duration: Duration::from_millis(1250),
        request_id: Some("gen-dec-1".into()),
    };

    let mut cursor = db.cursor().await.unwrap();
    cursor
        .add_events(
            agent.id,
            &[NewEvent::new(EventType::DecisionCall(call.clone()))],
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();

    let mut cursor = db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    let stored: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventType::DecisionCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(stored, vec![call]);
}

#[tokio::test]
async fn session_decision_models_are_updated_without_clearing_and_inherited_by_forks() {
    let (_dir, db, session, _agent) = fixture().await;
    assert_eq!(session.decision_model, None);

    let mut cursor = db.cursor().await.unwrap();
    let updated = cursor
        .update_session(
            session.id,
            SessionUpdate {
                decision_model: Some(decider("vendor/a")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.decision_model, Some(decider("vendor/a")));

    // An update that omits the field leaves it unchanged.
    let renamed = cursor
        .update_session(
            session.id,
            SessionUpdate {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.decision_model, Some(decider("vendor/a")));

    let inherited = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(inherited.session.decision_model, Some(decider("vendor/a")));

    let overridden = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: SessionUpdate {
                    decision_model: Some(decider("vendor/b")),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(overridden.session.decision_model, Some(decider("vendor/b")));
    cursor.commit().await.unwrap();

    let mut cursor = db.cursor().await.unwrap();
    let stored = cursor.get_session(session.id).await.unwrap();
    assert_eq!(stored.decision_model, Some(decider("vendor/a")));
}
