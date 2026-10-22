use super::{Hook, HookError};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::cancellation::Cancellation,
    prompts,
    sessions::Session,
};
use psa_api::events::{EventType, Message, MessageType, NewEvent};
use std::time::Duration;

/// Initializes a new root agent from its pinned definition and session snapshot.
pub struct DefaultStartupHook;

impl Hook for DefaultStartupHook {
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
        // Forks already have an inherited context, which must not be cleared.
        if context.agent().parent_id.is_some() {
            return Ok(());
        }

        let model = context
            .definition()
            .model_config
            .as_ref()
            .unwrap_or(&context.session().model_config)
            .clone();
        let events = [
            NewEvent::new(EventType::NewContext),
            NewEvent::new(EventType::Model(model)),
            NewEvent::new(EventType::Message(Message::text(
                MessageType::System,
                prompts::default_system_prompt(),
            ))),
        ];

        context.save_events(&events).await?;
        Ok(())
    }
}
