pub mod cancellation;
pub mod tools;

use crate::agents::{
    AgentError, AgentState,
    agent_manager::{AgentManagerMessage, AgentManagerState},
    publication::status_event,
};
use actors::UnboundedMailbox;
use cancellation::{Cancellation, run_cancellable};
use psa_api::events::EventType;
use std::time::Duration;

pub trait Hook: Send {
    fn cancellation_timeout(&self) -> Duration;
    fn run(
        &mut self,
        state: &mut AgentManagerState,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<(), AgentError>> + Send;
}

#[derive(Default)]
pub struct NoopHook;

impl Hook for NoopHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        _state: &mut AgentManagerState,
        _cancellation: Cancellation,
    ) -> Result<(), AgentError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct StopWithoutTools;

impl Hook for StopWithoutTools {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        state: &mut AgentManagerState,
        mut cancellation: Cancellation,
    ) -> Result<(), AgentError> {
        if cancellation.is_cancelled() || state.agent().state != AgentState::Running {
            return Ok(());
        }

        let mut cursor = state.database().cursor().await?;
        let response = cursor.load_last_inference_result(state.agent().id).await?;
        cursor.rollback().await?;

        if cancellation.is_cancelled() {
            return Ok(());
        }
        let Some(response) = response else {
            return Ok(());
        };

        if !response
            .items
            .iter()
            .any(|event| matches!(event.kind, EventType::ToolCall(_)))
        {
            state
                .persist_events(&[status_event(AgentState::Idle)])
                .await?;
        }

        Ok(())
    }
}

pub enum HookKind {
    Noop(NoopHook),
    StopWithoutTools(StopWithoutTools),
}

impl Hook for HookKind {
    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Noop(hook) => hook.cancellation_timeout(),
            Self::StopWithoutTools(hook) => hook.cancellation_timeout(),
        }
    }

    async fn run(
        &mut self,
        state: &mut AgentManagerState,
        cancellation: Cancellation,
    ) -> Result<(), AgentError> {
        match self {
            Self::Noop(hook) => hook.run(state, cancellation).await,
            Self::StopWithoutTools(hook) => hook.run(state, cancellation).await,
        }
    }
}

pub(crate) async fn run_hook(
    hook: &mut HookKind,
    state: &mut AgentManagerState,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> Result<bool, AgentError> {
    let (sender, cancellation) = Cancellation::channel();
    let grace = hook.cancellation_timeout();
    let mut session = state.session().clone();
    let (result, interrupted) = run_cancellable(
        hook.run(state, cancellation),
        sender,
        grace,
        &mut session,
        controls,
    )
    .await;
    state.update_session(session);
    result.ok_or(AgentError::HookTimeout)??;
    Ok(interrupted)
}

pub struct Hooks {
    pub before_request: HookKind,
    pub after_request: HookKind,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            before_request: HookKind::Noop(NoopHook),
            after_request: HookKind::StopWithoutTools(StopWithoutTools),
        }
    }
}
