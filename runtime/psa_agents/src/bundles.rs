//! Agent-runtime owners of constructed tools and hooks, retained across workers.
//!
//! Call `InstalledComponents::cleanup` after releasing invocation/worker references.
//! Cleanup is bounded and best effort. If its future is forcibly dropped, ordinary
//! concrete `Drop` implementations are the fallback: Drop cannot await. Bundles
//! owning resources must therefore still provide cancellation-safe concrete Drop.

#[cfg(any(test, feature = "testing"))]
#[path = "tests/bundles/testing.rs"]
pub mod testing;

pub use psa::api::BundleConfig;

use crate::{
    Agent,
    codemode_functions::{CodeModeEntry, CodeModeFunction, CodeModeListing},
    hooks::{CommandHook, DynHook, Hook, Hooks},
    tools::{DynTool, Tool, ToolId, Tools},
};

use futures_util::future::BoxFuture;
use psa::api;
use psa_store::Session;
use std::{collections::HashSet, future::Future, sync::Arc, time::Duration};

/// A single non-cloneable agent-runtime owner. Installation only registers already
/// constructed components; async resource shutdown belongs in cleanup.
pub trait Bundle: Send + Sync {
    /// Name used in cleanup diagnostics.
    fn label(&self) -> String;

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

/// Object-safe form of [`Bundle`], provided by a blanket impl.
pub trait DynBundle: Send + Sync + 'static {
    fn label(&self) -> String;
    fn install(
        &mut self,
        agent: &Agent,
        session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError>;
    fn cleanup(&mut self) -> BoxFuture<'_, Result<(), CleanupError>>;
    fn cleanup_timeout(&self) -> Duration;
}

impl<T: Bundle + 'static> DynBundle for T {
    fn label(&self) -> String {
        Bundle::label(self)
    }

    fn install(
        &mut self,
        agent: &Agent,
        session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        Bundle::install(self, agent, session, installer)
    }

    fn cleanup(&mut self) -> BoxFuture<'_, Result<(), CleanupError>> {
        Box::pin(Bundle::cleanup(self))
    }

    fn cleanup_timeout(&self) -> Duration {
        Bundle::cleanup_timeout(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStage {
    AgentCreate,
    SessionUpdate,
    BeforeRequest,
    AfterRequest,
}

/// Lengths of an installer's lists before a bundle installs.
#[derive(Debug, Clone, Copy)]
struct InstallMark {
    tools: usize,
    codemode_functions: usize,
    /// Agent create, session update, before request, after request, command.
    hooks: [usize; 5],
}

/// Accepts concrete instances, preserving shared handles constructed by a bundle.
#[derive(Default)]
pub struct Installer {
    tools: Vec<Box<dyn DynTool>>,
    codemode_functions: Vec<CodeModeEntry>,
    hooks: Hooks,
}

impl Installer {
    pub fn add_tool(&mut self, tool: impl Tool + 'static) -> Result<(), InstallError> {
        self.add_dyn_tool(Box::new(tool))
    }

    pub fn add_dyn_tool(&mut self, tool: Box<dyn DynTool>) -> Result<(), InstallError> {
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
        function: impl CodeModeFunction,
    ) -> Result<(), InstallError> {
        self.register_codemode_function(Box::new(function), CodeModeListing::SystemPrompt)
    }

    /// Register an internal function whose stub is returned only by `help`,
    /// keeping rarely used functions out of the system prompt.
    pub fn add_help_only_codemode_function(
        &mut self,
        function: impl CodeModeFunction,
    ) -> Result<(), InstallError> {
        self.register_codemode_function(Box::new(function), CodeModeListing::HelpOnly)
    }

    fn register_codemode_function(
        &mut self,
        function: Box<dyn CodeModeFunction>,
        listing: CodeModeListing,
    ) -> Result<(), InstallError> {
        if let Some(
            ToolId::CodeMode
            | ToolId::ResetCodeMode
            | ToolId::SetGoal
            | ToolId::SetGoalStatus
            | ToolId::RecordResult,
        ) = function.tool_id()
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

    /// Where the next bundle's registrations will begin.
    fn mark(&self) -> InstallMark {
        InstallMark {
            tools: self.tools.len(),
            codemode_functions: self.codemode_functions.len(),
            hooks: [
                self.hooks.agent_create.len(),
                self.hooks.session_update.len(),
                self.hooks.before_request.len(),
                self.hooks.after_request.len(),
                self.hooks.command.len(),
            ],
        }
    }

    /// Describe everything registered since `mark`, attributed to `config`.
    fn describe_since(&self, mark: InstallMark, config: &BundleConfig) -> api::InstalledBundle {
        use api::HookStage as Stage;

        let tools = self.tools[mark.tools..]
            .iter()
            .map(|tool| {
                let definition = tool.tool_definition();
                api::InstalledTool {
                    id: tool.id(),
                    name: definition.name,
                    description: definition.description,
                }
            })
            .collect();

        let hook = |stage, name: &str| api::InstalledHook {
            stage,
            name: name.into(),
        };
        let staged = [
            (Stage::AgentCreate, &self.hooks.agent_create),
            (Stage::SessionUpdate, &self.hooks.session_update),
            (Stage::BeforeRequest, &self.hooks.before_request),
            (Stage::AfterRequest, &self.hooks.after_request),
        ];
        let mut hooks: Vec<_> = staged
            .into_iter()
            .zip(mark.hooks)
            .flat_map(|((stage, list), start)| {
                list[start..].iter().map(move |h| hook(stage, h.name()))
            })
            .collect();
        hooks.extend(
            self.hooks.command[mark.hooks[4]..]
                .iter()
                .map(|h| hook(Stage::Command, h.name())),
        );

        let codemode_functions = self.codemode_functions[mark.codemode_functions..]
            .iter()
            .map(|entry| {
                let definition = entry.function.definition();
                api::InstalledCodeModeFunction {
                    name: definition.name,
                    description: definition.description,
                    help_only: matches!(entry.listing, CodeModeListing::HelpOnly),
                }
            })
            .collect();

        api::InstalledBundle {
            config: config.clone(),
            tools,
            hooks,
            codemode_functions,
        }
    }

    pub fn tools(&self) -> &[Box<dyn DynTool>] {
        &self.tools
    }

    pub fn codemode_functions(&self) -> &[CodeModeEntry] {
        &self.codemode_functions
    }

    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Test seam: remove the registered tools, to register them elsewhere.
    #[doc(hidden)]
    pub fn take_tools(&mut self) -> Vec<Box<dyn DynTool>> {
        std::mem::take(&mut self.tools)
    }

    /// Hooks are intentionally not deduplicated, even within one stage.
    pub fn add_hook(&mut self, stage: HookStage, hook: impl Hook + 'static) {
        self.add_dyn_hook(stage, Box::new(hook));
    }

    pub fn add_dyn_hook(&mut self, stage: HookStage, hook: Box<dyn DynHook>) {
        let hooks = match stage {
            HookStage::AgentCreate => &mut self.hooks.agent_create,
            HookStage::SessionUpdate => &mut self.hooks.session_update,
            HookStage::BeforeRequest => &mut self.hooks.before_request,
            HookStage::AfterRequest => &mut self.hooks.after_request,
        };
        hooks.push(hook);
    }

    pub fn add_command_hook(&mut self, hook: impl CommandHook + 'static) {
        self.hooks.command.push(Box::new(hook));
    }
}

#[derive(Default)]
pub struct InstalledComponents {
    pub tools: Tools,
    pub codemode_functions: Arc<[CodeModeEntry]>,
    pub hooks: Hooks,
    bundles: Vec<Box<dyn DynBundle>>,
    /// What each bundle installed, for clients inspecting the agent.
    summary: Vec<api::InstalledBundle>,
}

impl InstalledComponents {
    /// Construct and install one bundle per config, in order. A failed install
    /// still hands every owner created so far to rollback.
    pub async fn install(
        configs: &[BundleConfig],
        agent: &Agent,
        session: &Session,
        mut build: impl FnMut(&BundleConfig) -> Result<Box<dyn DynBundle>, InstallError>,
    ) -> Result<Self, InstallError> {
        api::validate_completion_modes(configs)
            .map_err(|error| InstallError::new(error.to_string()))?;
        let mut installer = Installer::default();
        let mut bundles = Vec::new();
        let mut summary = Vec::new();
        let mut install_all = || -> Result<(), InstallError> {
            let mut ids = HashSet::new();
            for config in configs {
                let id = config.id();
                if !ids.insert(id) {
                    return Err(InstallError::new(format!("duplicate bundle ID: {id:?}")));
                }
                // Retain even the failing owner, before it can register anything.
                bundles.push(build(config)?);
                let bundle = bundles.last_mut().unwrap();
                let mark = installer.mark();
                bundle.install(agent, session, &mut installer)?;
                summary.push(installer.describe_since(mark, config));
            }
            Ok(())
        };
        let result = install_all();

        Self::finish_install(installer, bundles, summary, result).await
    }

    /// Every installed component, grouped by its owning bundle.
    pub fn describe(&self) -> api::AgentComponents {
        api::AgentComponents {
            bundles: self.summary.clone(),
        }
    }

    async fn finish_install(
        installer: Installer,
        bundles: Vec<Box<dyn DynBundle>>,
        summary: Vec<api::InstalledBundle>,
        result: Result<(), InstallError>,
    ) -> Result<Self, InstallError> {
        let mut installed = Self {
            tools: installer.tools.into(),
            codemode_functions: installer.codemode_functions.into(),
            hooks: installer.hooks,
            bundles,
            summary,
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
    /// installation order. Every owner is attempted despite errors or timeouts.
    /// Draining ownership makes repeated calls harmless, including after errors.
    pub async fn cleanup(&mut self) -> Result<(), CleanupErrors> {
        self.tools = Arc::default();
        self.codemode_functions = Arc::default();
        self.hooks = Hooks::default();
        let mut errors = Vec::new();
        let mut bundles = std::mem::take(&mut self.bundles);
        for bundle in bundles.iter_mut().rev() {
            let label = bundle.label();
            let timeout = bundle.cleanup_timeout();
            let failure = match tokio::time::timeout(timeout, bundle.cleanup()).await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(CleanupFailureKind::Failed {
                    message: error.message,
                }),
                Err(_) => Some(CleanupFailureKind::TimedOut { timeout }),
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
}

#[derive(Debug, thiserror::Error)]
#[error("bundle cleanup failures: {errors:?}")]
pub struct CleanupErrors {
    pub errors: Vec<CleanupFailure>,
}
