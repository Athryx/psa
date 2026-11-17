//! Agent-runtime owners of constructed tools and hooks, retained across workers.
//!
//! Call `InstalledComponents::cleanup` after releasing invocation/worker references.
//! Cleanup is bounded and best effort. If its future is forcibly dropped, ordinary
//! concrete `Drop` implementations are the fallback: Drop cannot await. Bundles
//! owning resources must therefore still provide cancellation-safe concrete Drop.

mod ask_question;
pub mod bash;
pub mod codemode;
mod compaction;
mod completion;
pub(crate) mod decision_models;
mod file_tools;
pub mod goal;
mod noop;
pub mod session;
pub mod subagents;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
mod traces;
pub(crate) mod typed_result;

pub use ask_question::AskQuestionBundle;
pub use bash::BashBundle;
pub use codemode::CodeModeBundle;
pub use compaction::CompactionBundle;
pub use completion::{CompletionConfigError, validate_completion_config};
pub use decision_models::DecisionModelsBundle;
pub use file_tools::FileToolsBundle;
pub use goal::GoalBundle;
pub use noop::NoopBundle;
pub use psa::api::BundleConfig;
pub use subagents::SubagentsBundle;
pub use traces::TracesBundle;
pub use typed_result::TypedResultBundle;

use crate::{
    agents::{Agent, AgentDefinition},
    components::{
        codemode_functions::{
            CodeModeEntry, CodeModeFunction, CodeModeFunctionKind, CodeModeListing,
        },
        hooks::{CommandHookKind, HookKind, Hooks},
        tools::ToolKind,
    },
    sessions::Session,
};
use futures_util::FutureExt;
use std::{
    any::Any, collections::HashSet, future::Future, panic::AssertUnwindSafe, sync::Arc,
    time::Duration,
};

/// A single non-cloneable agent-runtime owner. Installation only registers already
/// constructed components; async resource shutdown belongs in cleanup.
pub trait Bundle: Send + Sync {
    fn install(
        &mut self,
        agent: &Agent,
        session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError>;

    fn cleanup(&mut self) -> impl Future<Output = Result<(), CleanupError>> + Send;

    fn cleanup_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }
}

/// Static Rust dispatch, not a dynamic registry or a second tool execution path.
pub enum BundleKind {
    Noop(NoopBundle),
    FileTools(FileToolsBundle),
    Bash(BashBundle),
    CodeMode(CodeModeBundle),
    Goal(GoalBundle),
    AskQuestion(AskQuestionBundle),
    TypedResult(TypedResultBundle),
    Subagents(SubagentsBundle),
    Compaction(CompactionBundle),
    Traces(TracesBundle),
    DecisionModels(DecisionModelsBundle),
    #[cfg(test)]
    Probe(testing::ProbeBundle),
}

impl BundleKind {
    pub fn from_config(
        config: &BundleConfig,
        resources: Option<&session::SessionBundleHandle>,
    ) -> Result<Self, InstallError> {
        let bundle = match config {
            BundleConfig::Noop => Self::Noop(NoopBundle),
            BundleConfig::FileTools { read_only } => {
                Self::FileTools(FileToolsBundle::new(*read_only))
            }
            BundleConfig::Bash => Self::Bash(BashBundle::new()),
            BundleConfig::CodeMode => Self::CodeMode(CodeModeBundle::new()),
            BundleConfig::Goal { interactive_only } => {
                Self::Goal(GoalBundle::new(*interactive_only))
            }
            BundleConfig::AskQuestion { interactive_only } => {
                Self::AskQuestion(AskQuestionBundle::new(*interactive_only))
            }
            BundleConfig::TypedResult {
                schema,
                max_retries,
            } => Self::TypedResult(TypedResultBundle::new(schema.clone(), *max_retries)?),
            BundleConfig::Subagents => {
                let resources = resources.ok_or_else(|| {
                    InstallError::new("Subagents requires session bundle resources")
                })?;
                Self::Subagents(SubagentsBundle::with_session(resources))
            }
            BundleConfig::Compaction {
                reserve_tokens,
                keep_recent_tokens,
                context_window,
            } => Self::Compaction(CompactionBundle::new(
                *reserve_tokens,
                *keep_recent_tokens,
                *context_window,
            )),
            BundleConfig::Traces => Self::Traces(TracesBundle),
            BundleConfig::DecisionModels {
                include_global,
                models,
            } => {
                let resources = resources.ok_or_else(|| {
                    InstallError::new("DecisionModels requires session bundle resources")
                })?;
                Self::DecisionModels(DecisionModelsBundle::new(
                    *include_global,
                    models,
                    resources,
                ))
            }
        };
        Ok(bundle)
    }

    fn label(&self) -> String {
        match self {
            Self::Noop(_) => "Noop".into(),
            Self::FileTools(_) => "FileTools".into(),
            Self::Bash(_) => "Bash".into(),
            Self::CodeMode(_) => "CodeMode".into(),
            Self::Goal(_) => "Goal".into(),
            Self::AskQuestion(_) => "AskQuestion".into(),
            Self::TypedResult(_) => "TypedResult".into(),
            Self::Subagents(_) => "Subagents".into(),
            Self::Compaction(_) => "Compaction".into(),
            Self::Traces(_) => "Traces".into(),
            Self::DecisionModels(_) => "DecisionModels".into(),
            #[cfg(test)]
            Self::Probe(bundle) => bundle.name.clone(),
        }
    }
}

impl Bundle for BundleKind {
    fn install(
        &mut self,
        agent: &Agent,
        session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        match self {
            Self::Noop(bundle) => bundle.install(agent, session, installer),
            Self::FileTools(bundle) => bundle.install(agent, session, installer),
            Self::Bash(bundle) => bundle.install(agent, session, installer),
            Self::CodeMode(bundle) => bundle.install(agent, session, installer),
            Self::Goal(bundle) => bundle.install(agent, session, installer),
            Self::AskQuestion(bundle) => bundle.install(agent, session, installer),
            Self::TypedResult(bundle) => bundle.install(agent, session, installer),
            Self::Subagents(bundle) => bundle.install(agent, session, installer),
            Self::Compaction(bundle) => bundle.install(agent, session, installer),
            Self::Traces(bundle) => bundle.install(agent, session, installer),
            Self::DecisionModels(bundle) => bundle.install(agent, session, installer),
            #[cfg(test)]
            Self::Probe(bundle) => bundle.install(agent, session, installer),
        }
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        match self {
            Self::Noop(bundle) => bundle.cleanup().await,
            Self::FileTools(bundle) => bundle.cleanup().await,
            Self::Bash(bundle) => bundle.cleanup().await,
            Self::CodeMode(bundle) => bundle.cleanup().await,
            Self::Goal(bundle) => bundle.cleanup().await,
            Self::AskQuestion(bundle) => bundle.cleanup().await,
            Self::TypedResult(bundle) => bundle.cleanup().await,
            Self::Subagents(bundle) => bundle.cleanup().await,
            Self::Compaction(bundle) => bundle.cleanup().await,
            Self::Traces(bundle) => bundle.cleanup().await,
            Self::DecisionModels(bundle) => bundle.cleanup().await,
            #[cfg(test)]
            Self::Probe(bundle) => bundle.cleanup().await,
        }
    }

    fn cleanup_timeout(&self) -> Duration {
        match self {
            Self::Noop(bundle) => bundle.cleanup_timeout(),
            Self::FileTools(bundle) => bundle.cleanup_timeout(),
            Self::Bash(bundle) => bundle.cleanup_timeout(),
            Self::CodeMode(bundle) => bundle.cleanup_timeout(),
            Self::Goal(bundle) => bundle.cleanup_timeout(),
            Self::AskQuestion(bundle) => bundle.cleanup_timeout(),
            Self::TypedResult(bundle) => bundle.cleanup_timeout(),
            Self::Subagents(bundle) => bundle.cleanup_timeout(),
            Self::Compaction(bundle) => bundle.cleanup_timeout(),
            Self::Traces(bundle) => bundle.cleanup_timeout(),
            Self::DecisionModels(bundle) => bundle.cleanup_timeout(),
            #[cfg(test)]
            Self::Probe(bundle) => bundle.cleanup_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStage {
    AgentCreate,
    SessionUpdate,
    BeforeRequest,
    AfterRequest,
}

/// Accepts concrete instances, preserving shared handles constructed by a bundle.
#[derive(Default)]
pub struct Installer {
    tools: Vec<ToolKind>,
    codemode_functions: Vec<CodeModeEntry>,
    hooks: Hooks,
}

impl Installer {
    pub fn add_tool(&mut self, tool: ToolKind) -> Result<(), InstallError> {
        let id = tool.id();
        let name = tool.tool_definition().name;
        for installed in &self.tools {
            if installed.id() == id {
                return Err(InstallError::new(format!("duplicate tool ID: {id:?}")));
            }
            if installed.tool_definition().name == name {
                return Err(InstallError::new(format!(
                    "duplicate tool API name: {name}"
                )));
            }
        }
        self.tools.push(tool);
        Ok(())
    }

    /// Register an internal function independently of model-facing tool names,
    /// with its stub listed in the system prompt.
    pub fn add_codemode_function(
        &mut self,
        function: CodeModeFunctionKind,
    ) -> Result<(), InstallError> {
        self.register_codemode_function(function, CodeModeListing::SystemPrompt)
    }

    /// Register an internal function whose stub is returned only by `help`,
    /// keeping rarely used functions out of the system prompt.
    pub fn add_help_only_codemode_function(
        &mut self,
        function: CodeModeFunctionKind,
    ) -> Result<(), InstallError> {
        self.register_codemode_function(function, CodeModeListing::HelpOnly)
    }

    fn register_codemode_function(
        &mut self,
        function: CodeModeFunctionKind,
        listing: CodeModeListing,
    ) -> Result<(), InstallError> {
        if let CodeModeFunctionKind::Tool(
            ToolKind::CodeMode(_)
            | ToolKind::ResetCodeMode(_)
            | ToolKind::SetGoal(_)
            | ToolKind::SetGoalStatus(_)
            | ToolKind::RecordResult(_),
        ) = &function
        {
            return Err(InstallError::new(
                "codemode cannot expose codemode, reset_codemode, goal, or result tools",
            ));
        }

        let name = function.definition().name;
        if self
            .codemode_functions
            .iter()
            .any(|existing| existing.function.definition().name == name)
        {
            return Err(InstallError::new(format!(
                "duplicate codemode function name: {name}"
            )));
        }

        self.codemode_functions
            .push(CodeModeEntry { function, listing });
        Ok(())
    }

    /// Hooks are intentionally not deduplicated, even within one stage.
    pub fn add_hook(&mut self, stage: HookStage, hook: HookKind) {
        let hooks = match stage {
            HookStage::AgentCreate => &mut self.hooks.agent_create,
            HookStage::SessionUpdate => &mut self.hooks.session_update,
            HookStage::BeforeRequest => &mut self.hooks.before_request,
            HookStage::AfterRequest => &mut self.hooks.after_request,
        };
        hooks.push(hook);
    }

    pub fn add_command_hook(&mut self, hook: CommandHookKind) {
        self.hooks.command.push(hook);
    }
}

/// Rejects a command at submission unless the definition selects the bundle
/// that installs its handler, instead of queueing work nobody would execute.
pub fn require_command_support(
    definition: &AgentDefinition,
    command: &psa::api::AgentCommand,
) -> Result<(), String> {
    let (name, bundle) = match command {
        psa::api::AgentCommand::Compact { .. } => ("Compact", psa::api::BundleId::Compaction),
    };
    if definition
        .bundles
        .iter()
        .any(|config| config.id() == bundle)
    {
        return Ok(());
    }
    Err(format!(
        "the {name} command requires the agent's {bundle:?} bundle"
    ))
}

#[derive(Default)]
pub struct InstalledComponents {
    pub(crate) tools: Arc<[ToolKind]>,
    pub(crate) codemode_functions: Arc<[CodeModeEntry]>,
    pub(crate) hooks: Hooks,
    bundles: Vec<BundleKind>,
}

impl InstalledComponents {
    pub async fn install(
        definition: &AgentDefinition,
        agent: &Agent,
        session: &Session,
    ) -> Result<Self, InstallError> {
        Self::install_with_session(definition, agent, session, None).await
    }

    pub async fn install_with_session(
        definition: &AgentDefinition,
        agent: &Agent,
        session: &Session,
        resources: Option<&session::SessionBundleHandle>,
    ) -> Result<Self, InstallError> {
        psa::api::validate_completion_modes(&definition.bundles, &definition.hooks)
            .map_err(|error| InstallError::new(error.to_string()))?;
        let child_templates = Self::load_child_templates(definition, resources).await?;
        let mut installer = Installer::default();
        let mut bundles = Vec::new();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut ids = HashSet::new();
            for config in &definition.bundles {
                let id = config.id();
                if !ids.insert(id) {
                    return Err(InstallError::new(format!("duplicate bundle ID: {id:?}")));
                }
                // Retain even the failing owner, before it can register anything.
                bundles.push(BundleKind::from_config(config, resources)?);
                if let BundleKind::Subagents(bundle) = bundles.last_mut().unwrap() {
                    bundle.set_child_templates(child_templates.clone());
                }
                bundles
                    .last_mut()
                    .unwrap()
                    .install(agent, session, &mut installer)?;
            }

            // Initialize context before bundle-owned creation prompts. Other
            // stages retain bundle-first ordering.
            let bundle_create_hooks = std::mem::take(&mut installer.hooks.agent_create);
            for (stage, ids) in [
                (HookStage::AgentCreate, &definition.hooks.agent_create),
                (HookStage::SessionUpdate, &definition.hooks.session_update),
                (HookStage::BeforeRequest, &definition.hooks.before_request),
                (HookStage::AfterRequest, &definition.hooks.after_request),
            ] {
                for config in ids {
                    installer.add_hook(stage, HookKind::from_config(config));
                }
            }
            installer.hooks.agent_create.extend(bundle_create_hooks);
            Ok(())
        }))
        .unwrap_or_else(|panic| {
            Err(InstallError::new(format!(
                "installation panicked: {}",
                panic_message(panic)
            )))
        });

        Self::finish_install(installer, bundles, result).await
    }

    /// Permitted child templates for the Subagents bundle, which installs synchronously.
    async fn load_child_templates(
        definition: &AgentDefinition,
        resources: Option<&session::SessionBundleHandle>,
    ) -> Result<Vec<subagents::ChildTemplate>, InstallError> {
        let uses_subagents = definition
            .bundles
            .iter()
            .any(|config| matches!(config, BundleConfig::Subagents));
        let Some(resources) = resources.filter(|_| uses_subagents) else {
            return Ok(Vec::new());
        };
        if definition.permitted_subagents.is_empty() {
            return Ok(Vec::new());
        }
        resources
            .subagents()
            .load_child_templates(&definition.permitted_subagents)
            .await
            .map_err(InstallError::new)
    }

    async fn finish_install(
        installer: Installer,
        bundles: Vec<BundleKind>,
        result: Result<(), InstallError>,
    ) -> Result<Self, InstallError> {
        let mut installed = Self {
            tools: installer.tools.into(),
            codemode_functions: installer.codemode_functions.into(),
            hooks: installer.hooks,
            bundles,
        };
        if let Err(mut error) = result {
            if let Err(cleanup_errors) = installed.cleanup().await {
                tracing::warn!(error = %cleanup_errors, install_error = %error, "bundle installation rollback failed");
                error.rollback_errors = Some(cleanup_errors);
            }
            return Err(error);
        }
        Ok(installed)
    }

    /// Releases owned component references first, then cleans owners in reverse
    /// installation order. Every owner is attempted despite errors or panics.
    /// Draining ownership makes repeated calls harmless, including after errors.
    pub async fn cleanup(&mut self) -> Result<(), CleanupErrors> {
        self.tools = Arc::default();
        self.codemode_functions = Arc::default();
        self.hooks = Hooks::default();
        let mut errors = Vec::new();
        let mut bundles = std::mem::take(&mut self.bundles);
        for bundle in bundles.iter_mut().rev() {
            let label = bundle.label();
            let outcome = AssertUnwindSafe(async {
                let timeout = bundle.cleanup_timeout();
                match tokio::time::timeout(timeout, bundle.cleanup()).await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(CleanupFailureKind::Failed {
                        message: error.message,
                    }),
                    Err(_) => Some(CleanupFailureKind::TimedOut { timeout }),
                }
            })
            .catch_unwind()
            .await;
            let failure = match outcome {
                Ok(failure) => failure,
                Err(panic) => Some(CleanupFailureKind::Panicked {
                    message: panic_message(panic),
                }),
            };
            if let Some(kind) = failure {
                errors.push(CleanupFailure {
                    bundle: label,
                    kind,
                });
            }
        }
        drop(bundles);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CleanupErrors { errors })
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct InstallError {
    pub message: String,
    /// The original installation failure remains primary; rollback failures are
    /// retained separately, never substituted for it.
    pub rollback_errors: Option<CleanupErrors>,
}

impl InstallError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rollback_errors: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CleanupError {
    pub message: String,
}

impl CleanupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CleanupFailure {
    pub bundle: String,
    pub kind: CleanupFailureKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CleanupFailureKind {
    Failed { message: String },
    TimedOut { timeout: Duration },
    Panicked { message: String },
}

#[derive(Debug, thiserror::Error)]
#[error("bundle cleanup failures: {errors:?}")]
pub struct CleanupErrors {
    pub errors: Vec<CleanupFailure>,
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).into()
    } else {
        "non-string panic payload".into()
    }
}
