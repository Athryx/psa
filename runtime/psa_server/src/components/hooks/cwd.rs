use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation, prompts};
use psa::api::events::{EventType, Message};
use std::time::Duration;

/// Adds the invocation's session working directory to the model context.
pub struct CwdHook;

impl Hook for CwdHook {
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
            .save_events(&[NewEvent::new(EventType::Message(Message::system(
                prompt,
                "Working directory",
            )))])
            .await?;
        Ok(())
    }
}
