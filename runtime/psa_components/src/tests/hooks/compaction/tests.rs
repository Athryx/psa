use super::*;
use psa::api::{
    AgentId,
    events::{EventId, Message},
};
use psa_agents::agent_projection::AgentProjection;

fn events(kinds: Vec<EventType>) -> Vec<Event> {
    kinds
        .into_iter()
        .enumerate()
        .map(|(index, kind)| Event {
            id: EventId(index as u64 + 1),
            source_agent_id: AgentId(1),
            timestamp: time::OffsetDateTime::now_utc(),
            kind,
        })
        .collect()
}

#[test]
fn summaries_require_complete_nonempty_text_not_partial_results() {
    use psa::api::events::InferenceResultEnd;
    for (outcome, accepted) in [
        (
            Outcome::Completed {
                reason: StopReason::EndTurn,
            },
            true,
        ),
        (
            Outcome::Completed {
                reason: StopReason::StopSequence,
            },
            true,
        ),
        (
            Outcome::Completed {
                reason: StopReason::TokenLimit,
            },
            false,
        ),
        (
            Outcome::Incomplete {
                reason: StopReason::TokenLimit,
            },
            false,
        ),
        (Outcome::Cancelled, false),
    ] {
        let transcript = events(vec![EventType::InferenceResultEnd(InferenceResultEnd {
            id: 1,
            outcome,
            usage: Default::default(),
            request_time: Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        })]);
        assert_eq!(
            require_summary(AgentResult::Text("checkpoint".into()), &transcript).is_ok(),
            accepted
        );
        assert!(require_summary(AgentResult::Text("  ".into()), &transcript).is_err());
        assert!(require_summary(AgentResult::Json(serde_json::json!({})), &transcript).is_err());
    }
}

#[test]
fn replacement_replays_system_prompts_tools_summary_and_tail_without_old_context() {
    let model = crate::test_support::model_config();
    let tool = crate::tools::tool_definition(psa::api::ToolId::ListSessions).unwrap();
    let original = events(vec![
        EventType::NewContext,
        EventType::Model(model.clone()),
        EventType::Message(Message::text(MessageType::System, "system one")),
        EventType::ToolDefinition(psa::api::events::ToolDefinition {
            id: psa::api::ToolId::ListSessions,
            definition: tool,
        }),
        EventType::Message(Message::text(MessageType::Human, "old".repeat(100))),
        EventType::Message(Message::text(MessageType::Agent, "old answer".repeat(100))),
        EventType::Message(Message::text(MessageType::System, "system two")),
        EventType::Message(Message::text(MessageType::Human, "recent")),
    ]);
    let prepared = prepare::prepare(&original, 2).unwrap();
    let replacement = build_replacement(&original, &model, prepared, "checkpoint".into());
    let mut kinds: Vec<_> = original.iter().map(|event| event.kind.clone()).collect();
    kinds.extend(replacement.into_iter().map(|event| event.kind));
    let projection = AgentProjection::replay(&events(kinds)).unwrap();
    let history = projection.build_history().unwrap();
    let text = serde_json::to_string(&history.items).unwrap();
    assert!(text.contains("system one"));
    assert!(text.contains("system two"));
    assert!(text.contains("checkpoint"));
    assert!(text.contains("recent"));
    assert!(!text.contains("old answer"));
    assert_eq!(projection.tool_definitions().len(), 1);
    assert_eq!(projection.model_config(), Some(&model));
    assert!(projection.last_response().is_none());
}
