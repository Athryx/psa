pub mod cancellation;
pub mod tools;

use crate::{
    agents::{
        AgentError, AgentState, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        publication::status_event, session_manager::RuntimeError,
    },
    db::DbError,
};
use actors::UnboundedMailbox;
use cancellation::{Cancellation, run_cancellable};
use psa_api::{events::EventType, probe_transport::ProbeError};
use std::time::Duration;

/// Hooks are part of the core loop and propagate failures from their capabilities.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Sessions(#[from] RuntimeError),
}

pub trait Hook: Send {
    fn cancellation_timeout(&self) -> Duration;
    fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<(), HookError>> + Send;
}

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

#[derive(Default)]
pub struct StopWithoutTools;

impl Hook for StopWithoutTools {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if cancellation.is_cancelled() || context.agent().state != AgentState::Running {
            return Ok(());
        }

        let response = context.load_last_inference_result().await?;
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
            context
                .save_events(&[status_event(AgentState::Idle)])
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
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> Result<(), HookError> {
        match self {
            Self::Noop(hook) => hook.run(context, cancellation).await,
            Self::StopWithoutTools(hook) => hook.run(context, cancellation).await,
        }
    }
}

pub(crate) async fn run_hook(
    hook: &mut impl Hook,
    context: &mut AgentContext<'_>,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> Result<bool, AgentError> {
    let (sender, cancellation) = Cancellation::channel();
    let grace = hook.cancellation_timeout();
    let mut session = context.session().clone();
    let (result, interrupted) = run_cancellable(
        hook.run(context, cancellation),
        sender,
        grace,
        &mut session,
        controls,
    )
    .await;

    context.update_session(session);
    context.finish_persistence().await?;

    result
        .ok_or(AgentError::HookTimeout)?
        .map_err(|error| AgentError::Hook(Box::new(error)))?;

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
