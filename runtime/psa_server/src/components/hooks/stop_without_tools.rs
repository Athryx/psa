use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{
    agents::{AgentState, agent_context::AgentContext, publication::status_event},
    components::cancellation::Cancellation,
};
use llm_api::api::{ContentBlock, Outcome, StopReason};
use psa::api::{
    AgentResult,
    events::{Event, EventType, MessageType},
};
use std::time::Duration;

#[cfg(test)]
mod tests;

pub(super) fn build_text_completion_events(items: &[Event]) -> [NewEvent; 2] {
    let text = items
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            EventType::Message(message) if message.message_type == MessageType::Agent => {
                Some(message)
            }
            _ => None,
        })
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    ContentBlock::Refusal(refusal) => Some(refusal.text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default();

    [
        NewEvent::new(EventType::Result(AgentResult::Text(text))),
        status_event(AgentState::Idle),
    ]
}

#[derive(Default)]
pub struct StopWithoutTools;

impl Hook for StopWithoutTools {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if cancellation.is_cancelled() || context.agent().state != AgentState::Running {
            return Ok(());
        }

        let response = context.load_last_inference_result().await?;
        let Some(response) = response else {
            return Ok(());
        };

        let EventType::InferenceResultEnd(end) = &response.end.kind else {
            return Ok(());
        };
        if !matches!(
            end.outcome,
            Outcome::Completed { .. }
                | Outcome::Incomplete {
                    reason: StopReason::TokenLimit
                }
        ) {
            return Ok(());
        }

        if response
            .items
            .iter()
            .any(|event| matches!(event.kind, EventType::ToolCall(_)))
        {
            return Ok(());
        }

        let events = build_text_completion_events(&response.items);

        // Completion is not cancellation cleanup. An interrupt received during
        // the trace read must win before starting the worker-owned status save.
        if cancellation.is_cancelled() {
            return Ok(());
        }
        context.save_events(&events).await?;

        Ok(())
    }
}
