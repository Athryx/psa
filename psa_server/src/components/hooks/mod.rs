pub mod cwd;
pub mod default_startup;
pub mod extra_system_prompt;
pub mod goal;
pub mod noop;
pub mod project_context;
pub mod session_model_update;
pub mod skills;
pub mod stop_without_tools;
pub(crate) mod subagent_completion;
pub mod subagent_identity;
pub mod tool_definitions;

#[cfg(test)]
mod tests;

pub use cwd::CwdHook;
pub use default_startup::DefaultStartupHook;
pub use extra_system_prompt::ExtraSystemPromptHook;
pub use goal::GoalHook;
pub use noop::NoopHook;
pub use project_context::ProjectContextHook;
pub use session_model_update::SessionModelUpdateHook;
pub use skills::SkillsHook;
pub use stop_without_tools::StopWithoutTools;
pub use subagent_identity::SubagentIdentity;
pub use tool_definitions::ToolDefinitionsHook;

use super::cancellation::{Cancellation, run_cancellable};
use crate::{
    agents::{
        Agent, AgentError, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        session_manager::RuntimeError,
    },
    db::DbError,
    sessions::Session,
};
use actors::UnboundedMailbox;
pub use psa_api::{HookId, HookIds};
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
    #[error(transparent)]
    ClientQuery(#[from] crate::agents::client_queries::ClientQueryError),
}

pub trait Hook: Send {
    /// Builds an agent-local instance without performing IO.
    fn new(agent: &Agent, session: &Session) -> Self
    where
        Self: Sized;

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
    SubagentIdentity(SubagentIdentity),
    #[cfg(test)]
    RecordLifecycle(crate::agents::session_manager::lifecycle_tests::RecordLifecycle),
    #[cfg(test)]
    BundleProbe(crate::components::bundles::testing::SharedHook),
}

impl HookKind {
    pub fn new(id: HookId, agent: &Agent, session: &Session) -> Self {
        match id {
            HookId::Noop => Self::Noop(NoopHook::new(agent, session)),
            HookId::StopWithoutTools => {
                Self::StopWithoutTools(StopWithoutTools::new(agent, session))
            }
            HookId::DefaultStartup => Self::DefaultStartup(DefaultStartupHook::new(agent, session)),
            HookId::ExtraSystemPrompt => {
                Self::ExtraSystemPrompt(ExtraSystemPromptHook::new(agent, session))
            }
            HookId::ProjectContext => Self::ProjectContext(ProjectContextHook::new(agent, session)),
            HookId::ToolDefinitions => {
                Self::ToolDefinitions(ToolDefinitionsHook::new(agent, session))
            }
            HookId::Cwd => Self::Cwd(CwdHook::new(agent, session)),
            HookId::Skills => Self::Skills(SkillsHook::new(agent, session)),
            HookId::SessionModelUpdate => {
                Self::SessionModelUpdate(SessionModelUpdateHook::new(agent, session))
            }
        }
    }
}

impl Hook for HookKind {
    fn new(agent: &Agent, session: &Session) -> Self {
        Self::new(HookId::Noop, agent, session)
    }

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
            Self::SubagentIdentity(hook) => hook.cancellation_timeout(),
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
            Self::SubagentIdentity(hook) => hook.run(context, cancellation).await,
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

#[derive(Default)]
pub struct Hooks {
    /// Runs only for a newly created agent, never when loading a worker.
    pub agent_create: Vec<HookKind>,
    /// Runs once per session update, including for idle agents.
    pub session_update: Vec<HookKind>,
    pub before_request: Vec<HookKind>,
    pub after_request: Vec<HookKind>,
}

impl Hooks {
    pub fn new(ids: &HookIds, agent: &Agent, session: &Session) -> Self {
        let build = |ids: &[HookId]| {
            ids.iter()
                .map(|id| HookKind::new(*id, agent, session))
                .collect()
        };

        Self {
            agent_create: build(&ids.agent_create),
            session_update: build(&ids.session_update),
            before_request: build(&ids.before_request),
            after_request: build(&ids.after_request),
        }
    }
}
