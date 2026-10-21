use super::{Hook, HookError, cancellation::Cancellation};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    sessions::Session,
};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

/// Adds the pinned definition's extra system prompt to the model context.
pub struct ExtraSystemPromptHook;

impl Hook for ExtraSystemPromptHook {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let prompt = &context.definition().extra_system_prompt;
        if prompt.is_empty() {
            return Ok(());
        }

        let event = NewEvent::new(EventType::Message(Message::text(
            MessageType::System,
            prompt.clone(),
        )));
        context.save_events(&[event]).await?;
        Ok(())
    }
}
