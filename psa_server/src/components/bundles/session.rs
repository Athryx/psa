//! Session-scoped owners. Installed agent bundles borrow handles, never ownership.
use super::{
    CleanupError,
    subagents::{CoordinatorRuntime, SubagentCoordinator},
};
use crate::agents::{lifecycle::AgentLifecycleEvent, sessions::Sessions};
use psa_api::SessionId;

#[derive(Clone)]
pub struct SessionBundleHandle {
    subagents: SubagentCoordinator,
}
impl SessionBundleHandle {
    pub fn subagents(&self) -> SubagentCoordinator {
        self.subagents.clone()
    }
}

pub struct SessionBundleResources {
    subagents: CoordinatorRuntime,
}
impl SessionBundleResources {
    pub fn start(session: SessionId, sessions: Sessions) -> Self {
        Self {
            subagents: CoordinatorRuntime::start(session, sessions),
        }
    }
    pub fn handle(&self) -> SessionBundleHandle {
        SessionBundleHandle {
            subagents: self.subagents.handle(),
        }
    }
    pub(crate) fn notify(&self, event: AgentLifecycleEvent) {
        self.subagents.notify(event);
    }
    pub async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.subagents.cleanup().await
    }
}
