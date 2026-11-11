//! Locating one agent-to-agent message on the other side of the conversation.
//!
//! The server assigns each delivered message a request ID. It is recorded in the
//! received message's source, and `send_message` and `create_agent` return it in
//! their results, so both sides are linked by ID rather than by content.
use crate::{RowId, Transcript};
use llm_api::api::ContentBlock;
use psa_api::{
    AgentId,
    events::{EventType, MessageType},
};
use serde::Deserialize;

/// Where to reveal a message in another agent's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageLink {
    /// In the sender: the call whose result returned `request_id`. Completion
    /// reports have no such call; callers then reveal the latest activity.
    Sent { request_id: String },
    /// In the recipient: the attributed message delivered by `request_id`.
    Received { request_id: String },
}

/// Delivery recorded in a `send_message` or `create_agent` result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SentMessage {
    pub agent_id: AgentId,
    pub request_id: String,
}

impl SentMessage {
    /// Reads the delivery from a successful tool result's JSON text.
    pub fn parse(content: &[ContentBlock]) -> Option<Self> {
        content.iter().find_map(|block| match block {
            ContentBlock::Text(text) => serde_json::from_str(&text.text).ok(),
            _ => None,
        })
    }
}

/// Tools whose results report a message delivery.
pub fn is_message_tool(tool_name: &str) -> bool {
    matches!(tool_name, "send_message" | "create_agent")
}

impl MessageLink {
    /// The newest loaded row this link points at, if any. `None` for a
    /// completion report, or when the row is outside the loaded history.
    pub fn find(&self, transcript: &Transcript) -> Option<RowId> {
        match self {
            Self::Sent { request_id } => {
                let call_id = find_row(transcript, |kind| match kind {
                    EventType::ToolResult(result) => SentMessage::parse(&result.content)
                        .filter(|sent| sent.request_id == *request_id)
                        .map(|_| result.call_id.clone()),
                    _ => None,
                })?
                .1;
                find_row(transcript, |kind| {
                    matches!(kind, EventType::ToolCall(call) if call.call_id == call_id)
                        .then_some(())
                })
                .map(|(id, ())| id)
            }
            Self::Received { request_id } => find_row(transcript, |kind| {
                let EventType::Message(message) = kind else {
                    return None;
                };
                (message.message_type == MessageType::Subagent
                    && message
                        .source
                        .as_ref()
                        .is_some_and(|source| source.request_id == *request_id))
                .then_some(())
            })
            .map(|(id, ())| id),
        }
    }
}

fn find_row<T>(
    transcript: &Transcript,
    mut select: impl FnMut(&EventType) -> Option<T>,
) -> Option<(RowId, T)> {
    (0..transcript.row_count()).rev().find_map(|index| {
        let id = transcript.row_id_at(index)?;
        let payload = transcript.row_payload(&id)?;
        select(payload.kind()).map(|value| (id, value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa_api::{
        PendingEvent, SessionId, StreamMessage,
        events::{Message, MessageSender, ToolCall, ToolResult},
    };

    fn transcript(kinds: Vec<EventType>) -> Transcript {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        for (index, kind) in kinds.into_iter().enumerate() {
            transcript.apply(StreamMessage::Upsert {
                pending: PendingEvent {
                    id: format!("{index:06}"),
                    revision: 1,
                    session_id: SessionId(1),
                    agent_id: AgentId(1),
                    kind,
                },
            });
        }
        transcript
    }

    fn call(id: &str) -> EventType {
        EventType::ToolCall(ToolCall {
            call_id: id.into(),
            tool_name: "send_message".into(),
            arguments: r#"{"agent_name":"worker","message":"same text"}"#.into(),
            metadata: Default::default(),
        })
    }

    fn result(id: &str, request: &str) -> EventType {
        let text = serde_json::json!({"status":"queued","agent_id":2,"request_id":request});
        EventType::ToolResult(ToolResult {
            call_id: id.into(),
            content: Message::text(MessageType::Agent, &text.to_string()).content,
            error: false,
            metadata: Default::default(),
        })
    }

    fn received(request: &str) -> EventType {
        EventType::Message(Message {
            source: Some(MessageSender {
                agent_id: Some(AgentId(1)),
                name: Some("sender".into()),
                request_id: request.into(),
            }),
            ..Message::text(MessageType::Subagent, "same text")
        })
    }

    #[test]
    fn sent_links_resolve_the_call_by_its_results_request_id() {
        let sender = transcript(vec![
            call("a"),
            result("a", "r1"),
            call("b"),
            result("b", "r2"),
        ]);
        let link = MessageLink::Sent {
            request_id: "r1".into(),
        };
        assert_eq!(link.find(&sender), sender.row_id_at(0));
        let missing = MessageLink::Sent {
            request_id: "r3".into(),
        };
        assert_eq!(missing.find(&sender), None);
        let ToolResult { content, .. } = match result("a", "r1") {
            EventType::ToolResult(result) => result,
            _ => unreachable!(),
        };
        assert_eq!(
            SentMessage::parse(&content),
            Some(SentMessage {
                agent_id: AgentId(2),
                request_id: "r1".into()
            })
        );
    }

    #[test]
    fn received_links_match_by_request_id_not_text() {
        let recipient = transcript(vec![received("r1"), received("r2")]);
        let link = MessageLink::Received {
            request_id: "r1".into(),
        };
        assert_eq!(link.find(&recipient), recipient.row_id_at(0));
        assert!(is_message_tool("create_agent") && !is_message_tool("read"));
    }
}
