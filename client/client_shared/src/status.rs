//! Per-agent status projected from a server snapshot and newer committed events.
use crate::Transcript;
use llm_api::providers::ModelConfig;
use psa_api::{AgentStatusSnapshot, ModelInfo};

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
        .status_snapshot
        .clone()
        .unwrap_or_else(AgentStatusSnapshot::default);
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

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::{
        api::{Outcome, StopReason, Usage},
        providers::{Effort, Model, ProviderType},
    };
    use psa_api::{
        AgentId, EventEnvelope, SessionId,
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
        transcript
            .committed
            .insert(1, event(1, EventType::Model(config("test"))));
        let model: ModelInfo = serde_json::from_value(serde_json::json!({
            "model": {"provider":"Openrouter", "api_name":"test"},
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
    fn live_events_advance_snapshot_and_replay_does_not_override_it() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.status_snapshot = Some(AgentStatusSnapshot {
            head: 10,
            model_config: Some(config("current")),
            context_tokens: Some(50),
        });
        transcript.apply(psa_api::StreamMessage::Committed {
            event: event(3, EventType::Model(config("old"))),
            pending_id: None,
        });
        transcript.apply(psa_api::StreamMessage::Committed {
            event: event(11, end(Some(99), None, None)),
            pending_id: None,
        });
        let snapshot = transcript.status_snapshot.as_ref().unwrap();
        assert_eq!(snapshot.head, 11);
        assert_eq!(snapshot.model_config, Some(config("current")));
        assert_eq!(agent_status(&transcript, &[]).context_tokens, Some(99));
    }

    #[test]
    fn snapshot_replay_live_reset_and_unknown_usage() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
        transcript.status_snapshot = Some(AgentStatusSnapshot {
            head: 10,
            model_config: Some(config("current")),
            context_tokens: Some(50),
        });
        transcript
            .committed
            .insert(3, event(3, EventType::Model(config("old"))));
        assert_eq!(
            agent_status(&transcript, &[]).model_config,
            Some(config("current"))
        );
        transcript
            .committed
            .insert(11, event(11, end(None, Some(80), Some(20))));
        assert_eq!(agent_status(&transcript, &[]).context_tokens, Some(100));
        transcript
            .committed
            .insert(12, event(12, end(None, Some(3), None)));
        assert_eq!(agent_status(&transcript, &[]).context_tokens, None);
        transcript
            .committed
            .insert(13, event(13, EventType::NewContext));
        let status = agent_status(&transcript, &[]);
        assert!(status.model_config.is_none());
        assert_eq!(status.context_tokens, None);
    }
}
