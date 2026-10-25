use super::{Hook, HookError};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};
use std::time::Duration;

#[derive(Default)]
pub struct NoopHook;

impl Hook for NoopHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        Ok(())
    }
}
