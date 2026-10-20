use super::*;

#[test]
fn wire_ids_timestamps_and_message_requests_have_one_representation() {
    let event = events::Event {
        id: events::EventId(3),
        source_agent_id: AgentId(7),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        kind: events::EventType::NewContext,
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["id"], 3);
    assert_eq!(json["source_agent_id"], 7);
    assert_eq!(json["timestamp"], "1970-01-01T00:00:00Z");
    let decoded: events::Event = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.id, event.id);
    let request = AgentRequest::Message {
        delivery: MessageDelivery::Steer,
        content: vec![],
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(!json.contains("replay_content"));
    assert!(serde_json::from_str::<AgentRequest>(&json).is_ok());
    assert!(
        serde_json::from_str::<CreateSession>(
            r#"{"name":"test","working_directory":"/work","secret":"not allowed"}"#
        )
        .is_err()
    );
}

#[test]
fn paths_and_client_base_urls_are_validated() {
    assert_eq!(
        routes::agent_path(routes::AGENT_EVENTS, SessionId(1), AgentId(2)),
        "/sessions/1/agents/2/events"
    );
    for invalid in [
        "invalid",
        "file:///tmp",
        "https://example.com/?secret=x",
        "https://example.com/#x",
    ] {
        assert!(Client::new(invalid).is_err());
    }
    assert!(Client::new("http://localhost:8080/prefix/").is_ok());
}
