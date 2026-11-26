//! Named, asynchronous children sharing a session-owned resource coordinator.
mod coordinator;
pub(crate) mod templates;
pub(crate) use coordinator::CoordinatorRuntime;
pub use coordinator::SubagentCoordinator;
pub(crate) use templates::ChildTemplate;

use super::{
    Bundle, CleanupError, HookStage, InstallError, Installer, session::SessionBundleHandle,
};
use crate::codemode_functions::ToolFunction;
use crate::hooks::subagent_identity::SubagentIdentity;
use crate::tools::subagents::{CreateAgent, ListAgents, SendMessage, WaitMessage};
use psa_agents::Agent;
use psa_store::Session;
use std::sync::Arc;

pub struct SubagentsBundle {
    handle: SubagentCoordinator,
    templates: Arc<[ChildTemplate]>,
}
impl SubagentsBundle {
    pub fn with_session(session: &SessionBundleHandle) -> Self {
        Self {
            handle: session.subagents(),
            templates: Arc::default(),
        }
    }
    /// Installation is synchronous, so the caller loads the pinned definition's
    /// permitted templates beforehand. An empty list disables create_agent.
    pub fn set_child_templates(&mut self, templates: Vec<ChildTemplate>) {
        self.templates = templates.into();
    }
}
impl Bundle for SubagentsBundle {
    fn label(&self) -> String {
        "Subagents".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        let handle = self.handle.clone();
        if !self.templates.is_empty() {
            installer.add_tool(CreateAgent::new(handle.clone(), self.templates.clone()))?;
            installer.add_codemode_function(ToolFunction::new(CreateAgent::new(
                handle.clone(),
                self.templates.clone(),
            )))?;
        }
        installer.add_tool(SendMessage::with_handle(handle.clone()))?;
        installer.add_tool(WaitMessage::with_handle(handle.clone()))?;
        installer.add_tool(ListAgents::with_handle(handle.clone()))?;
        installer
            .add_codemode_function(ToolFunction::new(SendMessage::with_handle(handle.clone())))?;
        installer
            .add_codemode_function(ToolFunction::new(WaitMessage::with_handle(handle.clone())))?;
        installer.add_codemode_function(ToolFunction::new(ListAgents::with_handle(handle)))?;
        installer.add_hook(
            HookStage::AgentCreate,
            SubagentIdentity::new(self.templates.clone()),
        );
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
