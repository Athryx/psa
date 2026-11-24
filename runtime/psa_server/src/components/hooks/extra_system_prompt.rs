use super::{Hook, HookError};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};
use psa::api::events::{EventType, Message};
use psa_store::NewEvent;
use std::time::Duration;

/// Adds its configured extra system prompt to the model context.
pub struct ExtraSystemPromptHook {
    prompt: String,
}

impl ExtraSystemPromptHook {
    pub fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

impl Hook for ExtraSystemPromptHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        // Forks inherit the prompt with the rest of their context.
        let prompt = &self.prompt;
        if prompt.is_empty() || context.agent().forked_from.is_some() {
            return Ok(());
        }

        let event = NewEvent::new(EventType::Message(Message::system(
            prompt.clone(),
            "Additional instructions",
        )));
        context.save_events(&[event]).await?;
        Ok(())
    }
}
