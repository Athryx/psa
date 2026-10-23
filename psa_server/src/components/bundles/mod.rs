//! Agent-runtime owners of constructed tools and hooks, retained across workers.
//!
//! Call `InstalledComponents::cleanup` after releasing invocation/worker references.
//! Cleanup is bounded and best effort. If its future is forcibly dropped, ordinary
//! concrete `Drop` implementations are the fallback: Drop cannot await. Bundles
//! owning resources must therefore still provide cancellation-safe concrete Drop.

pub mod bash;
mod file_tools;
pub mod goal;
pub mod session;
pub mod subagents;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

pub use bash::BashBundle;
pub use file_tools::FileToolsBundle;
pub use goal::GoalBundle;
pub use psa_api::BundleId;
pub use subagents::SubagentsBundle;

use crate::{
    agents::{Agent, AgentDefinition},
    components::{
        hooks::{HookKind, Hooks},
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
    FileTools(FileToolsBundle),
    Bash(BashBundle),
    Goal(GoalBundle),
    Subagents(SubagentsBundle),
    #[cfg(test)]
    Probe(testing::ProbeBundle),
}

impl BundleKind {
    pub fn new(id: BundleId) -> Self {
        match id {
            BundleId::FileTools => Self::FileTools(FileToolsBundle::new()),
            BundleId::Bash => Self::Bash(BashBundle::new()),
            BundleId::Goal => Self::Goal(GoalBundle::new()),
            BundleId::Subagents => Self::Subagents(SubagentsBundle::new()),
        }
    }

    fn with_session(id: BundleId, resources: Option<&session::SessionBundleHandle>) -> Self {
        match (id, resources) {
            (BundleId::Subagents, Some(resources)) => {
                Self::Subagents(SubagentsBundle::with_session(resources))
            }
            _ => Self::new(id),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::FileTools(_) => "FileTools".into(),
            Self::Bash(_) => "Bash".into(),
            Self::Goal(_) => "Goal".into(),
            Self::Subagents(_) => "Subagents".into(),
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
            Self::FileTools(bundle) => bundle.install(agent, session, installer),
            Self::Bash(bundle) => bundle.install(agent, session, installer),
            Self::Goal(bundle) => bundle.install(agent, session, installer),
            Self::Subagents(bundle) => bundle.install(agent, session, installer),
            #[cfg(test)]
            Self::Probe(bundle) => bundle.install(agent, session, installer),
        }
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        match self {
            Self::FileTools(bundle) => bundle.cleanup().await,
            Self::Bash(bundle) => bundle.cleanup().await,
            Self::Goal(bundle) => bundle.cleanup().await,
            Self::Subagents(bundle) => bundle.cleanup().await,
            #[cfg(test)]
            Self::Probe(bundle) => bundle.cleanup().await,
        }
    }

    fn cleanup_timeout(&self) -> Duration {
        match self {
            Self::FileTools(bundle) => bundle.cleanup_timeout(),
            Self::Bash(bundle) => bundle.cleanup_timeout(),
            Self::Goal(bundle) => bundle.cleanup_timeout(),
            Self::Subagents(bundle) => bundle.cleanup_timeout(),
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
}

#[derive(Default)]
pub struct InstalledComponents {
    pub(crate) tools: Arc<[ToolKind]>,
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
        let mut installer = Installer::default();
        let mut bundles = Vec::new();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut ids = HashSet::new();
            for id in &definition.bundles {
                if !ids.insert(*id) {
                    return Err(InstallError::new(format!("duplicate bundle ID: {id:?}")));
                }
                // Retain even the failing owner, before it can register anything.
                bundles.push(BundleKind::with_session(*id, resources));
                if let BundleKind::Subagents(bundle) = bundles.last_mut().unwrap() {
                    bundle.set_allow_create(!definition.permitted_subagents.is_empty());
                }
                bundles
                    .last_mut()
                    .unwrap()
                    .install(agent, session, &mut installer)?;
            }

            for id in &definition.tools {
                if let Some(bundle) = ToolKind::required_bundle(*id) {
                    return Err(InstallError::new(format!(
                        "tool {id:?} requires bundle {bundle:?}"
                    )));
                }
                installer.add_tool(ToolKind::new(*id, agent, session))?;
            }
            for (stage, ids) in [
                (HookStage::AgentCreate, &definition.hooks.agent_create),
                (HookStage::SessionUpdate, &definition.hooks.session_update),
                (HookStage::BeforeRequest, &definition.hooks.before_request),
                (HookStage::AfterRequest, &definition.hooks.after_request),
            ] {
                for id in ids {
                    installer.add_hook(stage, HookKind::new(*id, agent, session));
                }
            }
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

    async fn finish_install(
        installer: Installer,
        bundles: Vec<BundleKind>,
        result: Result<(), InstallError>,
    ) -> Result<Self, InstallError> {
        let mut installed = Self {
            tools: installer.tools.into(),
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
