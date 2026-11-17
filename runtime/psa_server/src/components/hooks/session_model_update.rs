use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};
use psa::api::events::EventType;
use std::time::Duration;

/// Re-resolves the definition's model policy against the updated session, so
/// `Global` models and efforts follow session changes while fixed ones stay put.
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
        let model = context
            .definition()
            .model
            .resolve(&context.session().model_config);
        if context.projection().model_config() == Some(&model) {
            return Ok(());
        }

        context
            .save_events(&[NewEvent::new(EventType::Model(model))])
            .await?;
        Ok(())
    }
}
