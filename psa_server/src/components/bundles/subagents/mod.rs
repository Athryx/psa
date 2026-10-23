//! Named, asynchronous children sharing a session-owned resource coordinator.
mod coordinator;
pub(crate) use coordinator::CoordinatorRuntime;
pub use coordinator::SubagentCoordinator;

use super::{
    Bundle, CleanupError, HookStage, InstallError, Installer, session::SessionBundleHandle,
};
use crate::components::hooks::{HookKind, subagent_identity::SubagentIdentity};
use crate::{
    agents::Agent,
    components::tools::{
        ToolKind,
        subagents::{CreateAgent, ListAgents, SendMessage, WaitMessage},
    },
    sessions::Session,
};

#[derive(Default)]
pub struct SubagentsBundle {
    handle: Option<SubagentCoordinator>,
    allow_create: bool,
}
impl SubagentsBundle {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_session(session: &SessionBundleHandle) -> Self {
        Self {
            handle: Some(session.subagents()),
            allow_create: false,
        }
    }
    /// Installation is synchronous: the pinned definition supplies this policy.
    pub fn set_allow_create(&mut self, allow: bool) {
        self.allow_create = allow;
    }
}
impl Bundle for SubagentsBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        let handle = self
            .handle
            .clone()
            .ok_or_else(|| InstallError::new("Subagents requires session bundle resources"))?;
        if self.allow_create {
            installer.add_tool(ToolKind::CreateAgent(CreateAgent::with_handle(
                handle.clone(),
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
        installer.add_hook(
            HookStage::BeforeRequest,
            HookKind::SubagentIdentity(SubagentIdentity::default()),
        );
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
