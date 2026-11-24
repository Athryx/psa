//! Replay of events after a round trip through the store.

use crate::agents::agent_projection::{AgentProjection, RequestError};
use llm_api::api::{self, HistoryItemKind, Outcome, StopReason, Usage};
use psa::api::events::*;
use psa_store::{Database, EventRange, NewEvent, test_support::session_fixture as fixture};
use std::time::Duration;

fn model() -> llm_api::providers::ModelConfig {
    psa_store::test_support::model_config()
}

fn usage() -> Usage {
    Usage {
        input_tokens: Some(0),
        ..Default::default()
    }
}

#[tokio::test]
async fn atomically_persists_and_replays_a_complete_result_block() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut cursor = db.write().await.unwrap();
    let saved = cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::Model(model())),
                NewEvent::new(EventType::Message(Message::text(
                    MessageType::Human,
                    "question",
                ))),
                NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
                    id: 1,
                    model_config: model(),
                    request_config: api::RequestConfig::default(),
                })),
                NewEvent::new(EventType::Message(Message::text(
                    MessageType::Agent,
                    "answer",
                ))),
                NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
                    id: 1,
                    outcome: Outcome::Completed {
                        reason: StopReason::EndTurn,
                    },
                    usage: usage(),
                    request_time: Duration::from_micros(10),
                    response_id: Some("response".into()),
                    provider_metadata: Default::default(),
                })),
            ],
        )
        .await
        .unwrap();
    let prefix = cursor
        .load_events(
            agent.id,
            EventRange {
                through: Some(saved.events[3].id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let incomplete = AgentProjection::replay(&prefix).unwrap();
    assert!(matches!(
        incomplete.prepare_request(api::RequestConfig::default()),
        Err(RequestError::OpenInferenceResult(1))
    ));
    let loaded = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    let state = AgentProjection::replay(&loaded).unwrap();
    assert_eq!(state.items().len(), 2);
    assert_eq!(
        state.last_response().unwrap().response_id.as_deref(),
        Some("response")
    );
    assert_eq!(state.last_response().unwrap().usage.input_tokens, Some(0));
    let request = state
        .prepare_request(api::RequestConfig::default())
        .unwrap();
    assert_eq!(request.history.items.len(), 2);
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn subagent_messages_persist_source_and_replay_as_attributed_user_messages() {
    let (directory, db, _session, root) = fixture().await;
    let source = psa::api::events::AgentMessageLink {
        request_id: "request-1".into(),
        sender: root.id,
        sender_name: "researcher".into(),
        recipient: root.id,
        recipient_name: "recipient".into(),
    };
    let mut message = Message::text(MessageType::Subagent, "findings");
    message.source = Some(source.clone());
    let mut cursor = db.write().await.unwrap();
    // Subagent messages, and only they, record a sender.
    for invalid in [
        Message::text(MessageType::Subagent, "unattributed"),
        Message {
            source: Some(source.clone()),
            ..Message::text(MessageType::Human, "forged")
        },
    ] {
        assert!(
            cursor
                .add_events(root.id, &[NewEvent::new(EventType::Message(invalid))])
                .await
                .is_err()
        );
    }
    cursor
        .add_events(root.id, &[NewEvent::new(EventType::Message(message))])
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    db.close().await;
    let db = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = db.read().await.unwrap();
    let events = cursor.load_all_events(root.id).await.unwrap();
    let EventType::Message(message) = &events[0].kind else {
        panic!("expected message")
    };
    assert_eq!(message.message_type, MessageType::Subagent);
    assert_eq!(message.source, Some(source));
    let projection = AgentProjection::replay(&events).unwrap();
    let HistoryItemKind::Message(projected) = &projection.items()[0].kind else {
        panic!("expected projected message")
    };
    assert_eq!(projected.role, api::Role::User);
    assert!(
        matches!(&projected.content[0], api::ContentBlock::Text(text) if text.text == "Message from agent \"researcher\":\n")
    );
    assert!(
        matches!(&projected.content[1], api::ContentBlock::Text(text) if text.text == "findings")
    );
    cursor.close().await.unwrap();
    db.close().await;
}
