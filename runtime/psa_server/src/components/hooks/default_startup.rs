use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation, prompts};
use psa::api::events::{EventType, Message};
use std::time::Duration;

/// Initializes a new root agent from its pinned definition and session snapshot,
/// optionally with the base assistant instructions.
pub struct DefaultStartupHook {
    base_prompt: bool,
}

impl DefaultStartupHook {
    pub fn new(base_prompt: bool) -> Self {
        Self { base_prompt }
    }
}

impl Hook for DefaultStartupHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        // Forks already have an inherited context, which must not be cleared.
        if context.agent().forked_from.is_some() {
            return Ok(());
        }

        let model = context
            .definition()
            .model
            .resolve(&context.session().model_config);
        let mut events = vec![
            NewEvent::new(EventType::NewContext),
            NewEvent::new(EventType::Model(model)),
        ];
        if self.base_prompt {
            events.push(NewEvent::new(EventType::Message(Message::system(
                prompts::default_system_prompt(),
                "Base instructions",
            ))));
        }

        context.save_events(&events).await?;
        Ok(())
    }
}
