//! Named, asynchronous children sharing a session-owned resource coordinator.
mod coordinator;
pub(crate) mod templates;
pub(crate) use coordinator::CoordinatorRuntime;
pub use coordinator::SubagentCoordinator;
pub(crate) use templates::ChildTemplate;

use super::{
    Bundle, CleanupError, HookStage, InstallError, Installer, session::SessionBundleHandle,
};
use crate::components::{
    codemode_functions::CodeModeFunctionKind,
    hooks::{HookKind, subagent_identity::SubagentIdentity},
};
use crate::{
    agents::Agent,
    components::tools::{
        ToolKind,
        subagents::{CreateAgent, ListAgents, SendMessage, WaitMessage},
    },
};
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
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        let handle = self.handle.clone();
        if !self.templates.is_empty() {
            installer.add_tool(ToolKind::CreateAgent(CreateAgent::new(
                handle.clone(),
                self.templates.clone(),
            )))?;
            installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::CreateAgent(
                CreateAgent::new(handle.clone(), self.templates.clone()),
            )))?;
        }
        installer.add_tool(ToolKind::SendMessage(SendMessage::with_handle(
            handle.clone(),
        )))?;
        installer.add_tool(ToolKind::WaitMessage(WaitMessage::with_handle(
            handle.clone(),
        )))?;
        installer.add_tool(ToolKind::ListAgents(ListAgents::with_handle(
            handle.clone(),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::SendMessage(
            SendMessage::with_handle(handle.clone()),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::WaitMessage(
            WaitMessage::with_handle(handle.clone()),
        )))?;
        installer.add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::ListAgents(
            ListAgents::with_handle(handle),
        )))?;
        installer.add_hook(
            HookStage::AgentCreate,
            HookKind::SubagentIdentity(SubagentIdentity::new(self.templates.clone())),
        );
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
