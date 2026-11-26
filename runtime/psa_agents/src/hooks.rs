//! Hook interfaces, the stages they run in, and their runners.

use crate::cancellation::{Cancellation, run_cancellable};
use crate::{
    AgentError, agent_context::AgentContext, agent_manager::AgentManagerMessage,
    supervisor::RuntimeError,
};
use actors::UnboundedMailbox;
use futures_util::future::BoxFuture;
pub use psa::api::{AgentCommand, CommandResult};
use psa_shared::probe_transport::ProbeError;
use psa_store::DbError;
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
    Runtime(#[from] RuntimeError),
    #[error("context compaction failed: {0}")]
    Compaction(String),
    #[error(transparent)]
    ClientQuery(#[from] crate::client_queries::ClientQueryError),
}

/// Receives typed agent commands. A hook returns `None` for commands it does not
/// handle, leaving them to later hooks; the first result stops dispatch. Hooks
/// do not record the command: the worker records it with the returned result.
pub trait CommandHook: Send {
    /// Implementation name shown to clients.
    fn name(&self) -> &'static str;
    fn cancellation_timeout(&self) -> Duration;
    fn handle_command(
        &mut self,
        context: &mut AgentContext<'_>,
        command: &AgentCommand,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<Option<CommandResult>, HookError>> + Send;
}

/// Object-safe form of [`CommandHook`], provided by a blanket impl.
pub trait DynCommandHook: Send + 'static {
    fn name(&self) -> &'static str;
    fn cancellation_timeout(&self) -> Duration;
    fn handle_command<'a>(
        &'a mut self,
        context: &'a mut AgentContext<'_>,
        command: &'a AgentCommand,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, Result<Option<CommandResult>, HookError>>;
}

impl<T: CommandHook + 'static> DynCommandHook for T {
    fn name(&self) -> &'static str {
        CommandHook::name(self)
    }

    fn cancellation_timeout(&self) -> Duration {
        CommandHook::cancellation_timeout(self)
    }

    fn handle_command<'a>(
        &'a mut self,
        context: &'a mut AgentContext<'_>,
        command: &'a AgentCommand,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, Result<Option<CommandResult>, HookError>> {
        Box::pin(CommandHook::handle_command(
            self,
            context,
            command,
            cancellation,
        ))
    }
}

pub trait Hook: Send {
    /// Implementation name shown to clients.
    fn name(&self) -> &'static str;
    fn cancellation_timeout(&self) -> Duration;
    fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<(), HookError>> + Send;
}

/// Object-safe form of [`Hook`], provided by a blanket impl.
pub trait DynHook: Send + 'static {
    fn name(&self) -> &'static str;
    fn cancellation_timeout(&self) -> Duration;
    fn run<'a>(
        &'a mut self,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, Result<(), HookError>>;
}

impl<T: Hook + 'static> DynHook for T {
    fn name(&self) -> &'static str {
        Hook::name(self)
    }

    fn cancellation_timeout(&self) -> Duration {
        Hook::cancellation_timeout(self)
    }

    fn run<'a>(
        &'a mut self,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, Result<(), HookError>> {
        Box::pin(Hook::run(self, context, cancellation))
    }
}

pub(crate) async fn run_hooks(
    hooks: &mut [Box<dyn DynHook>],
    context: &mut AgentContext<'_>,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> Result<bool, AgentError> {
    let snapshot = context.session().clone();
    let mut latest = snapshot.clone();
    for hook in hooks {
        // Every hook in this stage observes the same invocation snapshot, even
        // if control messages deliver another session update between callbacks.
        context.update_session(snapshot.clone());
        let result = run_hook(hook.as_mut(), context, controls).await;
        if context.session() != &snapshot {
            latest = context.session().clone();
        }
        context.update_session(latest.clone());
        if result? {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Run one hook as the worker would, honouring interrupts from `controls`.
#[doc(hidden)]
pub async fn run_hook(
    hook: &mut dyn DynHook,
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

/// Offers a command to each hook in order until one handles it. Returns that
/// hook's result, if any, and whether an interrupt arrived while a hook ran.
pub(crate) async fn run_command_hooks(
    hooks: &mut [Box<dyn DynCommandHook>],
    context: &mut AgentContext<'_>,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
    command: &AgentCommand,
) -> Result<(Option<CommandResult>, bool), AgentError> {
    for hook in hooks {
        let (sender, cancellation) = Cancellation::channel();
        let grace = hook.cancellation_timeout();
        let mut session = context.session().clone();
        let (result, interrupted) = run_cancellable(
            hook.handle_command(context, command, cancellation),
            sender,
            grace,
            &mut session,
            controls,
        )
        .await;

        context.update_session(session);
        context.finish_persistence().await?;

        let handled = result
            .ok_or(AgentError::HookTimeout)?
            .map_err(|error| AgentError::Hook(Box::new(error)))?;
        if interrupted || handled.is_some() {
            return Ok((handled, interrupted));
        }
    }

    Ok((None, false))
}

#[derive(Default)]
pub struct Hooks {
    /// Runs only for a newly created agent, never when loading a worker.
    pub agent_create: Vec<Box<dyn DynHook>>,
    /// Runs once per session update, including for idle agents.
    pub session_update: Vec<Box<dyn DynHook>>,
    pub before_request: Vec<Box<dyn DynHook>>,
    pub after_request: Vec<Box<dyn DynHook>>,
    /// Bundle-installed handlers for submitted agent commands.
    pub command: Vec<Box<dyn DynCommandHook>>,
}
