use super::{Hook, HookError};
use crate::prompts;
use psa::api::events::{EventType, Message};
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use psa_store::NewEvent;
use std::time::Duration;

/// Adds the invocation's session working directory to the model context.
pub struct CwdHook;

impl Hook for CwdHook {
    fn name(&self) -> &'static str {
        "Cwd"
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
            .save_events(&[NewEvent::new(EventType::Message(Message::system(
                prompt,
                "Working directory",
            )))])
            .await?;
        Ok(())
    }
}
