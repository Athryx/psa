use super::{Hook, HookError, cancellation::Cancellation};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    prompts,
    sessions::Session,
};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

/// Adds the invocation's session working directory to the model context.
pub struct CwdHook;

impl Hook for CwdHook {
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
        let prompt = prompts::cwd_system_prompt(&context.session().working_directory);
        context
            .save_events(&[NewEvent::new(EventType::Message(Message::text(
                MessageType::System,
                prompt,
            )))])
            .await?;
        Ok(())
    }
}
