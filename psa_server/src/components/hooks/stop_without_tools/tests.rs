use super::*;
use llm_api::api::{Reasoning, RefusalBlock};
use psa_api::{
    AgentId,
    events::{EventId, Message, Thinking},
};

fn event(kind: EventType) -> Event {
    Event {
        id: EventId(1),
        source_agent_id: AgentId(1),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        kind,
    }
}

fn result_text(items: &[Event]) -> String {
    let [result, idle] = build_text_completion_events(items);
    assert!(
        matches!(idle.kind, EventType::StatusChange(status) if status.new_status == AgentState::Idle)
    );
    let EventType::Result(AgentResult::Text(text)) = result.kind else {
        panic!("ordinary completion must record a text result");
    };
    text
}

#[test]
fn completion_uses_only_last_assistant_message_visible_blocks() {
    let mut last = Message::text(MessageType::Agent, "last ");
    last.content.push(ContentBlock::Refusal(RefusalBlock {
        text: "refusal ".into(),
    }));
    last.content.extend(crate::prompts::text("answer"));
    let items = [
        event(EventType::Message(Message::text(
            MessageType::Agent,
            "earlier answer",
        ))),
        event(EventType::Message(last)),
        event(EventType::Thinking(Thinking {
            contents: Reasoning {
                summary: vec!["private reasoning".into()],
                ..Default::default()
            },
            metadata: Default::default(),
        })),
        event(EventType::Message(Message::text(
            MessageType::Human,
            "not assistant text",
        ))),
    ];
    assert_eq!(result_text(&items), "last refusal answer");
}

#[test]
fn empty_last_assistant_message_does_not_fall_back_to_earlier_text() {
    let mut empty = Message::text(MessageType::Agent, "");
    empty.content.clear();
    let items = [
        event(EventType::Message(Message::text(
            MessageType::Agent,
            "earlier answer",
        ))),
        event(EventType::Message(empty)),
    ];
    assert_eq!(result_text(&items), "");
    assert_eq!(result_text(&[]), "");
}

#[test]
fn completion_preserves_refusal_only_answer() {
    let mut last = Message::text(MessageType::Agent, "");
    last.content = vec![ContentBlock::Refusal(RefusalBlock {
        text: "I cannot help with that.".into(),
    })];
    let items = [event(EventType::Message(last))];
    assert_eq!(result_text(&items), "I cannot help with that.");
}

#[test]
fn completion_retains_full_untruncated_unicode_artifact() {
    let text = "雪".repeat(100_000);
    let items = [event(EventType::Message(Message::text(
        MessageType::Agent,
        text.clone(),
    )))];
    assert_eq!(result_text(&items), text);
}
