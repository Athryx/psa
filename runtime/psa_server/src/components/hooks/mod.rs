pub mod codemode_functions;
pub mod compaction;
pub mod cwd;
pub mod default_startup;
pub mod extra_system_prompt;
pub mod goal;
pub mod noop;
pub mod project_context;
pub mod require_result;
pub mod session_model_update;
pub mod skills;
pub mod stop_without_tools;
pub(crate) mod subagent_completion;
pub mod subagent_identity;
pub mod tool_definitions;

#[cfg(test)]
mod tests;

pub use codemode_functions::CodeModeFunctionsHook;
pub use cwd::CwdHook;
pub use default_startup::DefaultStartupHook;
pub use extra_system_prompt::ExtraSystemPromptHook;
pub use goal::GoalHook;
pub use noop::NoopHook;
pub use project_context::ProjectContextHook;
pub use require_result::RequireResult;
pub use session_model_update::SessionModelUpdateHook;
pub use skills::SkillsHook;
pub use stop_without_tools::StopWithoutTools;
pub use subagent_identity::SubagentIdentity;
pub use tool_definitions::ToolDefinitionsHook;

use super::cancellation::{Cancellation, run_cancellable};
use crate::{
    agents::{
        AgentError, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        session_manager::RuntimeError,
    },
    db::DbError,
};
use actors::UnboundedMailbox;
pub use psa::api::{AgentCommand, CommandResult, HookConfig, HookConfigs};
use psa_shared::probe_transport::ProbeError;
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
    #[error("context compaction failed: {0}")]
    Compaction(String),
    #[error(transparent)]
    ClientQuery(#[from] crate::agents::client_queries::ClientQueryError),
}

/// Receives typed agent commands. A hook returns `None` for commands it does not
/// handle, leaving them to later hooks; the first result stops dispatch. Hooks
/// do not record the command: the worker records it with the returned result.
pub trait CommandHook: Send {
    fn cancellation_timeout(&self) -> Duration;
    fn handle_command(
        &mut self,
        context: &mut AgentContext<'_>,
        command: &AgentCommand,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<Option<CommandResult>, HookError>> + Send;
}

/// Static dispatch for bundle-installed command hooks.
pub enum CommandHookKind {
    Compaction(compaction::CompactionHook),
}

impl CommandHook for CommandHookKind {
    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Compaction(hook) => CommandHook::cancellation_timeout(hook),
        }
    }

    async fn handle_command(
        &mut self,
        context: &mut AgentContext<'_>,
        command: &AgentCommand,
        cancellation: Cancellation,
    ) -> Result<Option<CommandResult>, HookError> {
        match self {
            Self::Compaction(hook) => hook.handle_command(context, command, cancellation).await,
        }
    }
}

pub trait Hook: Send {
    fn cancellation_timeout(&self) -> Duration;
    fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<(), HookError>> + Send;
}

pub enum HookKind {
    Noop(NoopHook),
    StopWithoutTools(StopWithoutTools),
    DefaultStartup(DefaultStartupHook),
    ExtraSystemPrompt(ExtraSystemPromptHook),
    ProjectContext(ProjectContextHook),
    ToolDefinitions(ToolDefinitionsHook),
    Cwd(CwdHook),
    Skills(SkillsHook),
    SessionModelUpdate(SessionModelUpdateHook),
    Goal(GoalHook),
    Compaction(compaction::CompactionHook),
    RequireResult(RequireResult),
    SubagentIdentity(SubagentIdentity),
    CodeModeFunctions(CodeModeFunctionsHook),
    #[cfg(test)]
    RecordLifecycle(crate::agents::session_manager::lifecycle_tests::RecordLifecycle),
    #[cfg(test)]
    BundleProbe(crate::components::bundles::testing::SharedHook),
}

impl HookKind {
    /// Builds an agent-local instance from configuration without performing IO.
    pub fn from_config(config: &HookConfig) -> Self {
        match config {
            HookConfig::Noop => Self::Noop(NoopHook),
            HookConfig::StopWithoutTools => Self::StopWithoutTools(StopWithoutTools),
            HookConfig::DefaultStartup => Self::DefaultStartup(DefaultStartupHook),
            HookConfig::ExtraSystemPrompt { prompt } => {
                Self::ExtraSystemPrompt(ExtraSystemPromptHook::new(prompt.clone()))
            }
            HookConfig::ProjectContext => Self::ProjectContext(ProjectContextHook),
            HookConfig::ToolDefinitions => Self::ToolDefinitions(ToolDefinitionsHook),
            HookConfig::Cwd => Self::Cwd(CwdHook),
            HookConfig::Skills => Self::Skills(SkillsHook),
            HookConfig::SessionModelUpdate => Self::SessionModelUpdate(SessionModelUpdateHook),
        }
    }
}

impl Hook for HookKind {
    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Noop(hook) => hook.cancellation_timeout(),
            Self::StopWithoutTools(hook) => hook.cancellation_timeout(),
            Self::DefaultStartup(hook) => hook.cancellation_timeout(),
            Self::ExtraSystemPrompt(hook) => hook.cancellation_timeout(),
            Self::ProjectContext(hook) => hook.cancellation_timeout(),
            Self::ToolDefinitions(hook) => hook.cancellation_timeout(),
            Self::Cwd(hook) => hook.cancellation_timeout(),
            Self::Skills(hook) => hook.cancellation_timeout(),
            Self::SessionModelUpdate(hook) => hook.cancellation_timeout(),
            Self::Goal(hook) => hook.cancellation_timeout(),
            Self::Compaction(hook) => Hook::cancellation_timeout(hook),
            Self::RequireResult(hook) => hook.cancellation_timeout(),
            Self::SubagentIdentity(hook) => hook.cancellation_timeout(),
            Self::CodeModeFunctions(hook) => hook.cancellation_timeout(),
            #[cfg(test)]
            Self::RecordLifecycle(hook) => hook.cancellation_timeout(),
            #[cfg(test)]
            Self::BundleProbe(hook) => hook.cancellation_timeout(),
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
            Self::DefaultStartup(hook) => hook.run(context, cancellation).await,
            Self::ExtraSystemPrompt(hook) => hook.run(context, cancellation).await,
            Self::ProjectContext(hook) => hook.run(context, cancellation).await,
            Self::ToolDefinitions(hook) => hook.run(context, cancellation).await,
            Self::Cwd(hook) => hook.run(context, cancellation).await,
            Self::Skills(hook) => hook.run(context, cancellation).await,
            Self::SessionModelUpdate(hook) => hook.run(context, cancellation).await,
            Self::Goal(hook) => hook.run(context, cancellation).await,
            Self::Compaction(hook) => hook.run(context, cancellation).await,
            Self::RequireResult(hook) => hook.run(context, cancellation).await,
            Self::SubagentIdentity(hook) => hook.run(context, cancellation).await,
            Self::CodeModeFunctions(hook) => hook.run(context, cancellation).await,
            #[cfg(test)]
            Self::RecordLifecycle(hook) => hook.run(context, cancellation).await,
            #[cfg(test)]
            Self::BundleProbe(hook) => hook.run(context, cancellation).await,
        }
    }
}

pub(crate) async fn run_hooks(
    hooks: &mut [HookKind],
    context: &mut AgentContext<'_>,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> Result<bool, AgentError> {
    let snapshot = context.session().clone();
    let mut latest = snapshot.clone();
    for hook in hooks {
        // Every hook in this stage observes the same invocation snapshot, even
        // if control messages deliver another session update between callbacks.
        context.update_session(snapshot.clone());
        let result = run_hook(hook, context, controls).await;
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

/// Offers a command to each hook in order until one handles it. Returns that
/// hook's result, if any, and whether an interrupt arrived while a hook ran.
pub(crate) async fn run_command_hooks(
    hooks: &mut [CommandHookKind],
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
    pub agent_create: Vec<HookKind>,
    /// Runs once per session update, including for idle agents.
    pub session_update: Vec<HookKind>,
    pub before_request: Vec<HookKind>,
    pub after_request: Vec<HookKind>,
    /// Bundle-installed handlers for submitted agent commands.
    pub command: Vec<CommandHookKind>,
}

impl Hooks {
    pub fn from_configs(configs: &HookConfigs) -> Self {
        let build = |configs: &[HookConfig]| configs.iter().map(HookKind::from_config).collect();

        Self {
            agent_create: build(&configs.agent_create),
            session_update: build(&configs.session_update),
            before_request: build(&configs.before_request),
            after_request: build(&configs.after_request),
            command: Vec::new(),
        }
    }
}
