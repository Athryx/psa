use super::{Hook, HookError};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};
use psa_api::events::{EventType, NewEvent};
use std::time::Duration;

/// Follows session model changes unless the definition pins its own model.
pub struct SessionModelUpdateHook;

impl Hook for SessionModelUpdateHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if context.definition().model_config.is_some()
            || context.projection().model_config() == Some(&context.session().model_config)
        {
            return Ok(());
        }

        let model = context.session().model_config.clone();
        context
            .save_events(&[NewEvent::new(EventType::Model(model))])
            .await?;
        Ok(())
    }
}
