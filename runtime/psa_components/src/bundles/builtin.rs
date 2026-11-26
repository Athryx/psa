//! The built-in bundle set: maps each `BundleConfig` to its concrete owner.

use super::{
    AskQuestionBundle, BashBundle, CodeModeBundle, CompactionBundle, DecisionModelsBundle,
    DynBundle, ExtraSystemPromptBundle, FileToolsBundle, GoalBundle, InstallError,
    InstalledComponents, StartupBundle, StopWithoutToolsBundle, SubagentsBundle, TracesBundle,
    TypedResultBundle,
    session::{SessionBundleHandle, SessionBundleResources},
    subagents::ChildTemplate,
    validate_completion_config,
};
use futures_util::future::BoxFuture;
use psa::api::{AgentCommand, BundleConfig, BundleId, SessionId};
use psa_agents::{
    Agent, AgentDefinition,
    factory::{ComponentFactory, SessionComponents, SessionComponentsHandle},
    runtime_handle::RuntimeHandle,
};
use psa_store::Session;

/// Construct the owner for one bundle configuration. Subagents and
/// DecisionModels need the session's shared resources.
pub fn build_bundle(
    config: &BundleConfig,
    resources: Option<&SessionBundleHandle>,
    child_templates: &[ChildTemplate],
) -> Result<Box<dyn DynBundle>, InstallError> {
    let bundle: Box<dyn DynBundle> = match config {
        BundleConfig::Startup {
            base_prompt,
            project_context,
            cwd,
            skills,
        } => Box::new(StartupBundle::new(
            *base_prompt,
            *project_context,
            *cwd,
            *skills,
        )),
        BundleConfig::StopWithoutTools => Box::new(StopWithoutToolsBundle),
        BundleConfig::ExtraSystemPrompt { prompt } => {
            Box::new(ExtraSystemPromptBundle::new(prompt.clone()))
        }
        BundleConfig::FileTools { read_only } => Box::new(FileToolsBundle::new(*read_only)),
        BundleConfig::Bash => Box::new(BashBundle::new()),
        BundleConfig::CodeMode => Box::new(CodeModeBundle::new()),
        BundleConfig::Goal { interactive_only } => Box::new(GoalBundle::new(*interactive_only)),
        BundleConfig::AskQuestion { interactive_only } => {
            Box::new(AskQuestionBundle::new(*interactive_only))
        }
        BundleConfig::TypedResult {
            schema,
            max_retries,
        } => Box::new(TypedResultBundle::new(schema.clone(), *max_retries)?),
        BundleConfig::Subagents => {
            let resources = resources
                .ok_or_else(|| InstallError::new("Subagents requires session bundle resources"))?;
            let mut bundle = SubagentsBundle::with_session(resources);
            bundle.set_child_templates(child_templates.to_vec());
            Box::new(bundle)
        }
        BundleConfig::Compaction {
            reserve_tokens,
            keep_recent_tokens,
            context_window,
        } => Box::new(CompactionBundle::new(
            *reserve_tokens,
            *keep_recent_tokens,
            *context_window,
        )),
        BundleConfig::Traces => Box::new(TracesBundle),
        BundleConfig::DecisionModels {
            models,
            first_model_default,
        } => {
            let resources = resources.ok_or_else(|| {
                InstallError::new("DecisionModels requires session bundle resources")
            })?;
            Box::new(DecisionModelsBundle::new(
                models,
                *first_model_default,
                resources,
            ))
        }
    };
    Ok(bundle)
}

/// Install every bundle a definition selects.
pub async fn install_bundles(
    definition: &AgentDefinition,
    agent: &Agent,
    session: &Session,
    resources: Option<&SessionBundleHandle>,
) -> Result<InstalledComponents, InstallError> {
    let child_templates = load_child_templates(definition, resources).await?;
    InstalledComponents::install(&definition.bundles, agent, session, |config| {
        build_bundle(config, resources, &child_templates)
    })
    .await
}

/// Permitted child templates for the Subagents bundle, which installs synchronously.
async fn load_child_templates(
    definition: &AgentDefinition,
    resources: Option<&SessionBundleHandle>,
) -> Result<Vec<ChildTemplate>, InstallError> {
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

/// Rejects a command at submission unless the definition selects the bundle
/// that installs its handler for this agent, instead of queueing work nobody
/// would execute.
pub fn require_command_support(
    definition: &AgentDefinition,
    interactive: bool,
    command: &AgentCommand,
) -> Result<(), String> {
    let (name, bundle) = match command {
        AgentCommand::Compact { .. } => ("Compact", BundleId::Compaction),
        AgentCommand::Goal(_) => ("Goal", BundleId::Goal),
    };
    let installs_handler = |config: &BundleConfig| match config {
        // Interactive-only Goal installs only its stopping policy on
        // noninteractive agents, without the command handler.
        BundleConfig::Goal { interactive_only } => !interactive_only || interactive,
        _ => true,
    };
    if definition
        .bundles
        .iter()
        .any(|config| config.id() == bundle && installs_handler(config))
    {
        return Ok(());
    }
    Err(format!(
        "the {name} command requires the agent's {bundle:?} bundle"
    ))
}

/// String-error form of [`validate_completion_config`] for the factory seam.
pub fn validate_bundles(bundles: &[BundleConfig]) -> Result<(), String> {
    validate_completion_config(bundles).map_err(|error| error.to_string())
}

fn uses_subagents(definition: &AgentDefinition) -> bool {
    definition
        .bundles
        .iter()
        .any(|bundle| bundle.id() == BundleId::Subagents)
}

/// The component factory for the built-in bundles.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinComponents;

impl ComponentFactory for BuiltinComponents {
    fn validate_bundles(&self, bundles: &[BundleConfig]) -> Result<(), String> {
        validate_bundles(bundles)
    }

    fn require_command_support(
        &self,
        definition: &AgentDefinition,
        interactive: bool,
        command: &AgentCommand,
    ) -> Result<(), String> {
        require_command_support(definition, interactive, command)
    }

    /// Subagents and their parents share the session coordinator.
    fn needs_session_components(
        &self,
        definition: &AgentDefinition,
        parent: Option<&AgentDefinition>,
    ) -> bool {
        uses_subagents(definition) || parent.is_some_and(uses_subagents)
    }

    fn start_session(
        &self,
        session: SessionId,
        runtime: RuntimeHandle,
        providers: psa_models::ProviderRegistry,
    ) -> Box<dyn SessionComponents> {
        Box::new(SessionBundleResources::start(
            session,
            runtime,
            Some(providers),
        ))
    }

    fn install<'a>(
        &'a self,
        definition: &'a AgentDefinition,
        agent: &'a Agent,
        session: &'a Session,
        resources: Option<&'a SessionComponentsHandle>,
    ) -> BoxFuture<'a, Result<InstalledComponents, InstallError>> {
        Box::pin(async move {
            let resources = resources
                .map(|handle| {
                    handle.downcast_ref::<SessionBundleHandle>().ok_or_else(|| {
                        InstallError::new("session components were not started by this factory")
                    })
                })
                .transpose()?;
            install_bundles(definition, agent, session, resources).await
        })
    }
}
