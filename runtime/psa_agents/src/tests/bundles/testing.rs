//! Explicit, per-test bundle factories. No process-global test registration.
use super::*;
use crate::{
    agent_context::AgentContext,
    cancellation::Cancellation,
    hooks::{Hook, HookError},
    tools::{Tool, ToolOutput, typed_tool_execute},
};
use llm_api::api::ToolDefinition;
use psa::api::ToolId;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
pub struct ProbeState {
    events: Mutex<Vec<String>>,
    pub value: AtomicUsize,
    pub hook_panics: AtomicBool,
    pub hook_fails: AtomicBool,
}

impl ProbeState {
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    pub fn record(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

#[derive(Clone, Copy)]
pub enum CleanupBehavior {
    Ok,
    Error,
    Pending,
}

#[derive(Clone, Copy)]
pub enum InstallBehavior {
    Ok,
    Error,
}

pub struct ProbeBundle {
    pub name: String,
    pub state: Arc<ProbeState>,
    pub cleanup: CleanupBehavior,
    pub install: InstallBehavior,
    pub timeout: Duration,
    pub components: bool,
}

impl ProbeBundle {
    pub fn new(name: impl Into<String>, state: Arc<ProbeState>) -> Self {
        Self {
            name: name.into(),
            state,
            cleanup: CleanupBehavior::Ok,
            install: InstallBehavior::Ok,
            timeout: Duration::from_secs(3),
            components: false,
        }
    }
}

impl Bundle for ProbeBundle {
    fn label(&self) -> String {
        self.name.clone()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        if self.components {
            installer.add_tool(SharedTool {
                state: self.state.clone(),
                id: ToolId::Edit,
                name: "probe_first".into(),
            })?;
            installer.add_tool(SharedTool {
                state: self.state.clone(),
                id: ToolId::Read,
                name: "probe_second".into(),
            })?;
            installer.add_hook(
                HookStage::BeforeRequest,
                SharedHook {
                    state: self.state.clone(),
                    name: "BundleProbe",
                },
            );
        }
        match self.install {
            InstallBehavior::Ok => Ok(()),
            InstallBehavior::Error => Err(InstallError::new(format!("install:{}", self.name))),
        }
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.state.record(format!("cleanup:{}", self.name));
        match self.cleanup {
            CleanupBehavior::Ok => Ok(()),
            CleanupBehavior::Error => Err(CleanupError::new(format!("cleanup:{}", self.name))),
            CleanupBehavior::Pending => std::future::pending().await,
        }
    }

    fn cleanup_timeout(&self) -> Duration {
        self.timeout
    }
}

impl Drop for ProbeBundle {
    fn drop(&mut self) {
        self.state.record(format!("drop:{}", self.name));
    }
}

/// Same retention/rollback path as production, without inventing test BundleIds.
pub async fn install_owned(
    owners: Vec<Box<dyn DynBundle>>,
    agent: &Agent,
    session: &Session,
) -> Result<InstalledComponents, InstallError> {
    let mut bundles = Vec::new();
    let mut installer = Installer::default();
    let install_all = || -> Result<(), InstallError> {
        for bundle in owners {
            bundles.push(bundle);
            bundles
                .last_mut()
                .unwrap()
                .install(agent, session, &mut installer)?;
        }
        Ok(())
    };
    let result = install_all();
    InstalledComponents::finish_install(installer, bundles, Vec::new(), result).await
}

pub struct SharedTool {
    pub state: Arc<ProbeState>,
    pub id: ToolId,
    pub name: String,
}

/// A tool taking no arguments that succeeds without effects, for tests that
/// only need some tool. It reports `id` as its registered identity.
pub fn test_tool(name: &str, id: ToolId) -> SharedTool {
    SharedTool {
        state: Arc::default(),
        id,
        name: name.into(),
    }
}

impl SharedTool {
    pub fn increment(&self) {
        self.state.value.fetch_add(1, Ordering::SeqCst);
    }

    async fn execute_typed(
        &self,
        _arguments: SharedArgs,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        self.increment();
        ToolOutput::success(Vec::new())
    }
}

impl Drop for SharedTool {
    fn drop(&mut self) {
        self.state.record(format!("tool_drop:{}", self.name));
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SharedArgs {}

impl Tool for SharedTool {
    fn id(&self) -> ToolId {
        self.id
    }

    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::for_type::<SharedArgs>(&self.name, "Bundle test state", Some(true))
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_tool_execute!(SharedArgs, execute_typed);
}

pub struct SharedHook {
    pub state: Arc<ProbeState>,
    /// Reported as the hook's name, so tests can tell instances apart.
    pub name: &'static str,
}

impl SharedHook {
    pub fn increment(&self) {
        self.state.value.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for SharedHook {
    fn drop(&mut self) {
        self.state.record("hook_drop");
    }
}

impl Hook for SharedHook {
    fn name(&self) -> &'static str {
        self.name
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if self.state.hook_panics.load(Ordering::SeqCst) {
            panic!("bundle hook panicked");
        }
        if self.state.hook_fails.load(Ordering::SeqCst) {
            return Err(HookError::Probe(
                psa_shared::probe_transport::ProbeError::Protocol("bundle hook failed".into()),
            ));
        }
        self.increment();
        Ok(())
    }
}

/// A factory installing an empty bundle per selected config, for tests that
/// need a runtime but not any concrete component. Installation still applies
/// the runtime's own checks, such as duplicate bundle IDs. Commands need the
/// bundle that handles them, and agents selecting Subagents (or whose parent
/// does) get no-op session components, as with the built-in factory.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestComponents;

impl crate::factory::ComponentFactory for TestComponents {
    fn validate_bundles(&self, _bundles: &[BundleConfig]) -> Result<(), String> {
        Ok(())
    }

    fn require_command_support(
        &self,
        definition: &crate::AgentDefinition,
        _interactive: bool,
        command: &psa::api::AgentCommand,
    ) -> Result<(), String> {
        let bundle = match command {
            psa::api::AgentCommand::Compact { .. } => psa::api::BundleId::Compaction,
            psa::api::AgentCommand::Goal(_) => psa::api::BundleId::Goal,
        };
        if definition
            .bundles
            .iter()
            .any(|config| config.id() == bundle)
        {
            return Ok(());
        }
        Err(format!(
            "the command requires the agent's {bundle:?} bundle"
        ))
    }

    fn needs_session_components(
        &self,
        definition: &crate::AgentDefinition,
        parent: Option<&crate::AgentDefinition>,
    ) -> bool {
        let uses_subagents = |definition: &crate::AgentDefinition| {
            definition
                .bundles
                .iter()
                .any(|bundle| bundle.id() == psa::api::BundleId::Subagents)
        };
        uses_subagents(definition) || parent.is_some_and(uses_subagents)
    }

    fn start_session(
        &self,
        _session: psa::api::SessionId,
        _runtime: crate::runtime_handle::RuntimeHandle,
        _providers: psa_models::ProviderRegistry,
    ) -> Box<dyn crate::factory::SessionComponents> {
        Box::new(TestSessionComponents)
    }

    fn install<'a>(
        &'a self,
        definition: &'a crate::AgentDefinition,
        agent: &'a Agent,
        session: &'a Session,
        _resources: Option<&'a crate::factory::SessionComponentsHandle>,
    ) -> BoxFuture<'a, Result<InstalledComponents, InstallError>> {
        Box::pin(InstalledComponents::install(
            &definition.bundles,
            agent,
            session,
            |config| Ok(Box::new(EmptyBundle(format!("{:?}", config.id())))),
        ))
    }
}

/// A stored definition for a tool taking no arguments, for transcripts that
/// only need some tool to exist under `id`.
pub fn tool_snapshot(id: ToolId, name: &str) -> psa::api::events::ToolDefinition {
    psa::api::events::ToolDefinition {
        id,
        definition: ToolDefinition::for_type::<SharedArgs>(name, "Test tool", Some(true)),
    }
}

/// Session components that hold nothing.
pub struct TestSessionComponents;

impl crate::factory::SessionComponents for TestSessionComponents {
    fn handle(&self) -> crate::factory::SessionComponentsHandle {
        crate::factory::SessionComponentsHandle::new(())
    }

    fn notify(&self, _event: crate::lifecycle::AgentLifecycleEvent) {}

    fn cleanup(&mut self) -> BoxFuture<'_, Result<(), CleanupError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A bundle that installs nothing, labelled with its config's bundle ID.
pub struct EmptyBundle(pub String);

impl Bundle for EmptyBundle {
    fn label(&self) -> String {
        self.0.clone()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        _installer: &mut Installer,
    ) -> Result<(), InstallError> {
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
