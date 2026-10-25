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
        let model = match &context.definition().model_config {
            Some(model) if context.projection().model_config().is_none() => model,
            Some(_) => return Ok(()),
            None => &context.session().model_config,
        };
        if context.projection().model_config() == Some(model) {
            return Ok(());
        }

        let model = model.clone();
        context
            .save_events(&[NewEvent::new(EventType::Model(model))])
            .await?;
        Ok(())
    }
}
