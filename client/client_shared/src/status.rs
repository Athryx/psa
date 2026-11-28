//! Per-agent status projected from a server snapshot and newer committed events.
use crate::Transcript;
use llm_api::providers::{Model, ModelConfig};
use psa::api::ModelInfo;

#[derive(Debug, Clone, Default)]
pub struct AgentStatus {
    pub model_config: Option<ModelConfig>,
    /// Last inference usage, not an exact measure of current context size.
    pub context_tokens: Option<u64>,
    /// Catalog model context-window limit, if available.
    pub context_window: Option<u64>,
}

pub fn agent_status(transcript: &Transcript, models: &[ModelInfo]) -> AgentStatus {
    let Some(agent_id) = transcript.agent_id else {
        return AgentStatus::default();
    };
    let mut snapshot = transcript
        .context_state
        .as_ref()
        .unwrap_or_else(|| transcript.fallback_context_state())
        .clone();
    for (_, envelope) in transcript.committed.range((
        std::ops::Bound::Excluded(snapshot.head),
        std::ops::Bound::Unbounded,
    )) {
        if envelope.agent_id == agent_id && envelope.session_id == transcript.session_id {
            snapshot.apply_event(&envelope.event);
        }
    }
    let context_window = snapshot.model_config.as_ref().and_then(|config| {
        models
            .iter()
            .find(|model| model.model == config.model)
            .and_then(|model| model.context_window)
    });
    AgentStatus {
        model_config: snapshot.model_config,
        context_tokens: snapshot.context_tokens,
        context_window,
    }
}

/// Catalog display name for a model, falling back to its API model name.
pub fn model_display_name(models: &[ModelInfo], model: &Model) -> String {
    models
        .iter()
        .find(|info| info.model == *model)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| model.model_name().to_owned())
}

/// Last inference usage against the context window, as the composer status
/// shows it: `(12% / 200k)`, with `?` for what is unknown.
pub fn format_context_usage(tokens: Option<u64>, window: Option<u64>) -> String {
    let percent = match (tokens, window) {
        (Some(tokens), Some(window)) if window > 0 => {
            format!(
                "{}%",
                (u128::from(tokens) * 100 + u128::from(window) / 2) / u128::from(window)
            )
        }
        _ => "?".to_string(),
    };
    let window = window
        .filter(|window| *window > 0)
        .map(format_token_count)
        .unwrap_or_else(|| "?".to_string());
    format!("({percent} / {window})")
}

/// A token count rounded to thousands, such as `200k`.
pub fn format_token_count(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    format!("{}k", (u128::from(count) + 500) / 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::{
        api::{Outcome, StopReason, Usage},
        providers::{Effort, Model, ProviderType},
    };
    use psa::api::{
        AgentContextState, AgentId, EventEnvelope, EventsPage, PendingEvent, SessionId,
        StreamMessage,
        events::{Event, EventId, EventType, InferenceResultEnd},
    };

    fn event(id: u64, kind: EventType) -> EventEnvelope {
        EventEnvelope {
            session_id: SessionId(1),
            agent_id: AgentId(1),
            position: id,
            event: Event {
                id: EventId(id),
                source_agent_id: AgentId(1),
                timestamp: time::OffsetDateTime::UNIX_EPOCH,
                kind,
            },
        }
    }
    fn config(name: &str) -> ModelConfig {
        ModelConfig {
            model: Model::new(ProviderType::Openrouter, name).unwrap(),
            effort: Effort::High,
        }
    }
    fn end(total: Option<u64>, input: Option<u64>, output: Option<u64>) -> EventType {
        EventType::InferenceResultEnd(InferenceResultEnd {
            id: 1,
            outcome: Outcome::Completed {
                reason: StopReason::EndTurn,
            },
            usage: Usage {
                total_tokens: total,
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
            request_time: std::time::Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        })
    }
    #[test]
    fn catalog_limit_matches_full_model_identity_and_unknown_is_preserved() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.apply(StreamMessage::Committed {
            event: event(1, EventType::Model(config("test"))),
            pending_id: None,
        });
        let model: ModelInfo = serde_json::from_value(serde_json::json!({
            "model": "openrouter/test",
            "name":"Test", "input_modalities":[], "context_window":1000,
            "max_output_tokens":null, "cost":{"tiers":[]}, "reasoning":false,
            "reasoning_options":[], "authed":true, "pinned":false,
            "refreshed_at":"2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            agent_status(&transcript, &[model]).context_window,
            Some(1000)
        );
        assert_eq!(agent_status(&transcript, &[]).context_window, None);
    }

    #[test]
    fn fallback_rebuilds_in_event_order_when_older_model_history_arrives() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.apply(StreamMessage::Committed {
            event: event(20, end(Some(100), None, None)),
            pending_id: None,
        });
        let first = agent_status(&transcript, &[]);
        assert!(first.model_config.is_none());
        assert_eq!(first.context_tokens, Some(100));
        let before_history = transcript.clone();
        transcript.merge_history(EventsPage {
            events: vec![event(10, EventType::Model(config("late-model")))],
            pending: Vec::new(),
            queries: Vec::new(),
            next_after: 20,
            next_before: None,
            has_older: false,
            head: 20,
            has_more: false,
        });
        let status = agent_status(&transcript, &[]);
        assert_eq!(status.model_config, Some(config("late-model")));
        assert_eq!(status.context_tokens, Some(100));
        assert_eq!(transcript.fallback_context_state().head, 20);
        assert!(!std::ptr::eq(
            before_history.fallback_context_state(),
            transcript.fallback_context_state()
        ));
        assert!(transcript.context_state.is_none());
    }

    #[test]
    fn pending_only_clones_and_duplicate_commits_share_initialized_fallback() {
        let mut original = Transcript::new(SessionId(1), Some(AgentId(1)));
        original.apply(StreamMessage::Committed {
            event: event(1, EventType::Model(config("cached"))),
            pending_id: None,
        });
        let cached = original.fallback_context_state();
        let mut transcript = original.clone();
        transcript.apply(StreamMessage::Upsert {
            pending: PendingEvent {
                id: "pending".into(),
                revision: 1,
                session_id: SessionId(1),
                agent_id: AgentId(1),
                kind: EventType::NewContext,
            },
        });
        assert!(std::ptr::eq(cached, transcript.fallback_context_state()));
        transcript.apply(StreamMessage::Snapshot {
            cursor: 1,
            pending: Vec::new(),
            queries: Vec::new(),
        });
        assert!(std::ptr::eq(cached, transcript.fallback_context_state()));
        transcript.apply(StreamMessage::Removed {
            session_id: SessionId(1),
            agent_id: AgentId(1),
            id: "pending".into(),
        });
        transcript.apply(StreamMessage::Committed {
            event: event(1, EventType::NewContext),
            pending_id: None,
        });
        assert!(std::ptr::eq(cached, transcript.fallback_context_state()));
        assert_eq!(
            agent_status(&transcript, &[]).model_config,
            Some(config("cached"))
        );
        transcript.apply(StreamMessage::Committed {
            event: event(2, EventType::NewContext),
            pending_id: None,
        });
        assert!(!std::ptr::eq(cached, transcript.fallback_context_state()));
        assert!(agent_status(&transcript, &[]).model_config.is_none());
    }

    #[test]
    fn live_events_advance_snapshot_and_replay_does_not_override_it() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.context_state = Some(AgentContextState {
            head: 10,
            model_config: Some(config("current")),
            context_tokens: Some(50),
        });
        transcript.apply(psa::api::StreamMessage::Committed {
            event: event(3, EventType::Model(config("old"))),
            pending_id: None,
        });
        transcript.apply(psa::api::StreamMessage::Committed {
            event: event(11, end(Some(99), None, None)),
            pending_id: None,
        });
        let snapshot = transcript.context_state.as_ref().unwrap();
        assert_eq!(snapshot.head, 11);
        assert_eq!(snapshot.model_config, Some(config("current")));
        assert_eq!(agent_status(&transcript, &[]).context_tokens, Some(99));
    }

    #[test]
    fn snapshot_replay_live_reset_and_unknown_usage() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.context_state = Some(AgentContextState {
            head: 10,
            model_config: Some(config("current")),
            context_tokens: Some(50),
        });
        transcript.apply(StreamMessage::Committed {
            event: event(3, EventType::Model(config("old"))),
            pending_id: None,
        });
        assert_eq!(
            agent_status(&transcript, &[]).model_config,
            Some(config("current"))
        );
        transcript.apply(StreamMessage::Committed {
            event: event(11, end(None, Some(80), Some(20))),
            pending_id: None,
        });
        assert_eq!(agent_status(&transcript, &[]).context_tokens, Some(100));
        transcript.apply(StreamMessage::Committed {
            event: event(12, end(None, Some(3), None)),
            pending_id: None,
        });
        assert_eq!(agent_status(&transcript, &[]).context_tokens, None);
        transcript.apply(StreamMessage::Committed {
            event: event(13, EventType::NewContext),
            pending_id: None,
        });
        let status = agent_status(&transcript, &[]);
        assert!(status.model_config.is_none());
        assert_eq!(status.context_tokens, None);
    }
}
