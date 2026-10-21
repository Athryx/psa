use super::{Hook, HookError, cancellation::Cancellation};
use crate::{
    agents::{Agent, AgentState, agent_context::AgentContext, publication::status_event},
    sessions::Session,
};
use psa_api::events::EventType;
use std::time::Duration;

#[derive(Default)]
pub struct StopWithoutTools;

impl Hook for StopWithoutTools {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        // no need to check cancellation, short runtime
        if context.agent().state != AgentState::Running {
            return Ok(());
        }

        let response = context.load_last_inference_result().await?;
        let Some(response) = response else {
            return Ok(());
        };

        if !response
            .items
            .iter()
            .any(|event| matches!(event.kind, EventType::ToolCall(_)))
        {
            context
                .save_events(&[status_event(AgentState::Idle)])
                .await?;
        }

        Ok(())
    }
}
