use super::*;
use crate::{EventRange, NewEvent};

#[tokio::test]
async fn codemode_definition_round_trips_through_database() {
    let (_dir, db, _session, agent) = fixture().await;
    let mut definition = tool_definition(ToolId::ListSessions);
    definition.id = ToolId::CodeMode;
    let mut cursor = db.write().await.unwrap();
    cursor
        .add_events(
            agent.id,
            &[NewEvent::new(EventType::ToolDefinition(definition))],
        )
        .await
        .unwrap();
    let events = cursor
        .load_events(agent.id, EventRange::default())
        .await
        .unwrap();
    assert!(
        matches!(&events[0].kind, EventType::ToolDefinition(value) if value.id == ToolId::CodeMode)
    );
    cursor.commit().await.unwrap();
}
