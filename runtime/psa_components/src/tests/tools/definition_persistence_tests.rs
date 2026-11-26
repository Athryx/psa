//! Native tool definitions survive persistence unchanged.

use crate::ToolDefinitionExt;
use psa::api::{ToolId, events::*};
use psa_store::{Database, NewEvent, test_support::session_fixture as fixture};

#[tokio::test]
async fn native_tool_definitions_round_trip_through_persistence_and_reopen() {
    let (directory, db, _session, agent) = fixture().await;
    let ids = [
        ToolId::Read,
        ToolId::Edit,
        ToolId::Write,
        ToolId::Bash,
        ToolId::InteractBash,
        ToolId::CloseBash,
        ToolId::SetGoal,
        ToolId::SetGoalStatus,
        ToolId::CreateAgent,
        ToolId::SendMessage,
        ToolId::WaitMessage,
        ToolId::ListAgents,
        ToolId::AskQuestion,
    ];
    assert!(super::tool_definition(ToolId::RecordResult).is_none());
    assert!(ToolDefinition::for_tool(ToolId::RecordResult).is_none());
    let events: Vec<_> = ids
        .iter()
        .map(|id| {
            NewEvent::new(EventType::ToolDefinition(
                ToolDefinition::for_tool(*id).unwrap(),
            ))
        })
        .collect();
    let mut cursor = db.write().await.unwrap();
    cursor.add_events(agent.id, &events).await.unwrap();
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(directory.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.read().await.unwrap();
    let saved = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(saved.len(), ids.len());
    for (event, id) in saved.iter().zip(ids) {
        let EventType::ToolDefinition(snapshot) = &event.kind else {
            panic!("expected tool definition");
        };
        assert_eq!(snapshot.id, id);
        assert_eq!(
            serde_json::to_value(&snapshot.definition).unwrap(),
            serde_json::to_value(super::tool_definition(id).unwrap()).unwrap(),
        );
    }
    cursor.close().await.unwrap();
    reopened.close().await;
}
