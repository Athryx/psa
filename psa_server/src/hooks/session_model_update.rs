use super::{Hook, HookError, cancellation::Cancellation};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    sessions::Session,
};
use psa_api::events::{EventType, NewEvent};
use std::time::Duration;

/// Follows session model changes unless the definition pins its own model.
pub struct SessionModelUpdateHook;

impl Hook for SessionModelUpdateHook {
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
