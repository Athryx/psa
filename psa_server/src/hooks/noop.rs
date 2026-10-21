use super::{Hook, HookError, cancellation::Cancellation};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    sessions::Session,
};
use std::time::Duration;

#[derive(Default)]
pub struct NoopHook;

impl Hook for NoopHook {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

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
