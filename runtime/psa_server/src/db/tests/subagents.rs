use super::*;
use crate::agents::AgentId;
use crate::db::NewEvent;

fn child(parent: AgentId, name: &str) -> NewAgent {
    NewAgent {
        subagent_parent_id: Some(parent),
        definition_id: psa_api::AgentDefinitionId(1),
        name: name.into(),
        interactive: false,
        hidden: false,
    }
}

#[tokio::test]
async fn ancestry_is_durable_distinct_from_fork_lineage_and_remapped_on_fork() {
    let (directory, db, session, root) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let worker = cursor
        .create_agent(session.id, child(root.id, "worker"))
        .await
        .unwrap();
    let nested = cursor
        .create_agent(session.id, child(worker.id, "nested"))
        .await
        .unwrap();
    assert_eq!(worker.subagent_parent_id, Some(root.id));
    assert_eq!(worker.parent_id, None);
    assert_eq!(
        cursor.list_agents(session.id).await.unwrap()[2].subagent_parent_id,
        Some(worker.id)
    );
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
    let copy = |id| {
        fork.agents
            .iter()
            .find(|agent| agent.parent_id == Some(id))
            .unwrap()
    };
    assert_eq!(copy(root.id).subagent_parent_id, None);
    assert_eq!(copy(worker.id).subagent_parent_id, Some(copy(root.id).id));
    assert_eq!(copy(nested.id).subagent_parent_id, Some(copy(worker.id).id));
    assert_eq!(copy(worker.id).parent_id, Some(worker.id));
    let copied_nested = copy(nested.id).id;
    let copied_worker = copy(worker.id).id;
    cursor.commit().await.unwrap();
    db.close().await;

    let db = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_agent(nested.id)
            .await
            .unwrap()
            .subagent_parent_id,
        Some(worker.id)
    );
    assert_eq!(
        cursor
            .get_agent(copied_nested)
            .await
            .unwrap()
            .subagent_parent_id,
        Some(copied_worker)
    );
    cursor.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn child_creation_rejects_missing_cross_session_parents_and_duplicate_names() {
    let (_directory, db, session, root) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
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
    let other = fork.agents[0].id;
    assert!(
        cursor
            .create_agent(session.id, child(other, "cross-session"))
            .await
            .is_err()
    );
    assert!(
        cursor
            .create_agent(session.id, child(AgentId(u32::MAX.into()), "missing"))
            .await
            .is_err()
    );
    assert!(
        cursor
            .create_agent(session.id, child(root.id, "main"))
            .await
            .is_err()
    );
    assert_eq!(cursor.list_agents(session.id).await.unwrap().len(), 1);
    // Root names remain compatible with existing API behavior.
    let mut duplicate_root = child(root.id, "main");
    duplicate_root.subagent_parent_id = None;
    cursor
        .create_agent(session.id, duplicate_root)
        .await
        .unwrap();
    cursor.commit().await.unwrap();
}

#[tokio::test]
async fn concurrent_child_creation_reserves_names_in_the_creation_transaction() {
    let (_directory, db, session, root) = fixture().await;
    let create = || async {
        let mut cursor = db.cursor().await.unwrap();
        let result = cursor
            .create_agent(session.id, child(root.id, "worker"))
            .await;
        cursor.commit().await.unwrap();
        result
    };
    let (first, second) = tokio::join!(create(), create());
    assert_ne!(first.is_ok(), second.is_ok());
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(cursor.list_agents(session.id).await.unwrap().len(), 2);
    cursor.rollback().await.unwrap();
}

#[tokio::test]
async fn subagent_messages_persist_source_and_replay_as_attributed_user_messages() {
    let (directory, db, _session, root) = fixture().await;
    let source = psa_api::events::AgentMessageLink {
        request_id: "request-1".into(),
        sender: root.id,
        sender_name: "researcher".into(),
        recipient: root.id,
        recipient_name: "recipient".into(),
    };
    let mut message = Message::text(MessageType::Subagent, "findings");
    message.source = Some(source.clone());
    let mut cursor = db.cursor().await.unwrap();
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
    let mut cursor = db.cursor().await.unwrap();
    let events = cursor.load_all_events(root.id).await.unwrap();
    let EventType::Message(message) = &events[0].kind else {
        panic!("expected message")
    };
    assert_eq!(message.message_type, MessageType::Subagent);
    assert_eq!(message.source, Some(source));
    let projection = crate::agents::agent_projection::AgentProjection::replay(&events).unwrap();
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
    cursor.rollback().await.unwrap();
    db.close().await;
}
