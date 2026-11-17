//! A minimal index of the user's messages, overlaid on a transcript's corner.
//! Ticks stay visible; hovering or focusing them lists message previews.
use client_shared::*;
use dioxus::prelude::*;
use llm_api::api::ContentBlock;
use psa::api::events::EventType;

/// Older turns stay reachable by scrolling; the overlay itself stays bounded.
const MAX_ENTRIES: usize = 200;
const PREVIEW_CHARS: usize = 96;

/// A rough indication of message length, drawn as the width of its tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavWeight {
    Short,
    Medium,
    Long,
}

impl NavWeight {
    fn from_text_bytes(bytes: usize) -> Self {
        match bytes {
            0..80 => Self::Short,
            80..400 => Self::Medium,
            _ => Self::Long,
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Medium => "medium",
            Self::Long => "long",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NavEntry {
    pub id: RowId,
    /// Display position, comparable with a mounted row's `data-row-index`.
    pub position: usize,
    pub preview: String,
    pub weight: NavWeight,
}

fn build_preview(content: &[ContentBlock]) -> (String, NavWeight) {
    let text_bytes = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.len(),
            _ => 0,
        })
        .sum();
    let first_line = content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => text.text.lines().map(str::trim).find(|l| !l.is_empty()),
        _ => None,
    });

    let preview = match first_line {
        Some(line) => match line.char_indices().nth(PREVIEW_CHARS) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line.to_owned(),
        },
        None if content.is_empty() => "(empty message)".to_owned(),
        None => "(attachment)".to_owned(),
    };
    (preview, NavWeight::from_text_bytes(text_bytes))
}

pub(crate) fn build_nav_entries(
    transcript: &Transcript,
    preferences: &UiPreferences,
) -> Vec<NavEntry> {
    let ids = transcript.human_message_ids();
    ids[ids.len().saturating_sub(MAX_ENTRIES)..]
        .iter()
        .filter_map(|id| {
            let payload = transcript.row_payload(id)?;
            let EventType::Message(message) = payload.kind() else {
                return None;
            };
            let (preview, weight) = build_preview(&message.content);
            Some(NavEntry {
                id: id.clone(),
                position: transcript.display_position(id, preferences)?,
                preview,
                weight,
            })
        })
        .collect()
}

#[component]
pub(crate) fn MessageNavigator(
    entries: Memo<Vec<NavEntry>>,
    onjump: EventHandler<RowId>,
) -> Element {
    let entries = entries.read();
    if entries.is_empty() {
        return rsx! {};
    }
    rsx! {
        nav { class: "message-nav", "data-transcript-nav": "true", "aria-label": "Your messages",
            div { class: "message-nav-ticks", "aria-hidden": "true",
                for entry in entries.iter() {
                    span { key: "{entry.position}", class: entry.weight.class(), "data-nav-index": entry.position }
                }
            }
            div { class: "message-nav-list",
                for entry in entries.iter().cloned() {
                    button {
                        key: "{entry.position}", r#type: "button", "data-nav-index": entry.position,
                        // Pointer use must not leave focus (and so the list) behind.
                        onmousedown: move |event| event.prevent_default(),
                        onclick: move |_| onjump.call(entry.id.clone()),
                        "{entry.preview}"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa::api::events::{Message, MessageType};
    use psa::api::{AgentId, PendingEvent, SessionId, StreamMessage};

    fn pending(id: usize, kind: EventType) -> PendingEvent {
        PendingEvent {
            id: format!("{id:06}"),
            revision: 1,
            session_id: SessionId(1),
            agent_id: AgentId(1),
            kind,
        }
    }

    fn message(role: MessageType, text: &str) -> EventType {
        EventType::Message(Message::text(role, text))
    }

    #[test]
    fn entries_list_only_human_messages_with_display_positions_and_bounded_previews() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        let long = format!("\n  {}\nsecond line", "é".repeat(PREVIEW_CHARS + 20));
        for (id, kind) in [
            message(MessageType::System, "system"),
            message(MessageType::Human, "first question"),
            message(MessageType::Agent, "answer"),
            message(MessageType::Human, &long),
            EventType::Message(Message {
                content: Vec::new(),
                ..Message::text(MessageType::Human, "")
            }),
        ]
        .into_iter()
        .enumerate()
        {
            transcript.apply(StreamMessage::Upsert {
                pending: pending(id, kind),
            });
        }

        let entries = build_nav_entries(&transcript, &UiPreferences::default());
        let summary: Vec<_> = entries
            .iter()
            .map(|entry| (entry.position, entry.weight))
            .collect();
        assert_eq!(
            summary,
            [
                (1, NavWeight::Short),
                (3, NavWeight::Medium),
                (4, NavWeight::Short)
            ]
        );
        assert_eq!(entries[0].preview, "first question");
        assert_eq!(entries[1].preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(entries[1].preview.starts_with('é') && entries[1].preview.ends_with('…'));
        assert_eq!(entries[2].preview, "(empty message)");
    }

    #[test]
    fn entries_keep_only_the_most_recent_messages() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        for id in 0..MAX_ENTRIES + 25 {
            transcript.apply(StreamMessage::Upsert {
                pending: pending(id, message(MessageType::Human, &format!("message {id}"))),
            });
        }
        let entries = build_nav_entries(&transcript, &UiPreferences::default());
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].position, 25);
        assert_eq!(
            entries.last().unwrap().preview,
            format!("message {}", MAX_ENTRIES + 24)
        );
    }
}
