//! Explicit, per-test bundle factories. No process-global test registration.
use super::*;
use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        hooks::{Hook, HookError},
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
};
use llm_api::api::ToolDefinition;
use psa_api::ToolId;
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
    Panic,
    Pending,
}

#[derive(Clone, Copy)]
pub enum InstallBehavior {
    Ok,
    Error,
    Panic,
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
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        if self.components {
            installer.add_tool(ToolKind::BundleProbe(SharedTool {
                state: self.state.clone(),
                id: ToolId::Noop,
                name: "probe_first".into(),
            }))?;
            installer.add_tool(ToolKind::BundleProbe(SharedTool {
                state: self.state.clone(),
                id: ToolId::Read,
                name: "probe_second".into(),
            }))?;
            installer.add_hook(
                HookStage::BeforeRequest,
                HookKind::BundleProbe(SharedHook {
                    state: self.state.clone(),
                }),
            );
        }
        match self.install {
            InstallBehavior::Ok => Ok(()),
            InstallBehavior::Error => Err(InstallError::new(format!("install:{}", self.name))),
            InstallBehavior::Panic => panic!("install:{}", self.name),
        }
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.state.record(format!("cleanup:{}", self.name));
        match self.cleanup {
            CleanupBehavior::Ok => Ok(()),
            CleanupBehavior::Error => Err(CleanupError::new(format!("cleanup:{}", self.name))),
            CleanupBehavior::Panic => panic!("cleanup:{}", self.name),
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
pub(crate) async fn install_owned(
    owners: Vec<BundleKind>,
    agent: &Agent,
    session: &Session,
) -> Result<InstalledComponents, InstallError> {
    let mut bundles = Vec::new();
    let mut installer = Installer::default();
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        for bundle in owners {
            bundles.push(bundle);
            bundles
                .last_mut()
                .unwrap()
                .install(agent, session, &mut installer)?;
        }
        Ok(())
    }))
    .unwrap_or_else(|panic| {
        Err(InstallError::new(format!(
            "installation panicked: {}",
            panic_message(panic)
        )))
    });
    InstalledComponents::finish_install(installer, bundles, result).await
}

pub struct SharedTool {
    pub state: Arc<ProbeState>,
    pub id: ToolId,
    pub name: String,
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
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self {
            state: Arc::default(),
            id: ToolId::Noop,
            name: "probe_first".into(),
        }
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
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self {
            state: Arc::default(),
        }
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
