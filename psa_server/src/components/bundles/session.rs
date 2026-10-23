//! Session-scoped owners. Installed agent bundles borrow handles, never ownership.
use super::{
    CleanupError,
    subagents::{CoordinatorRuntime, SubagentCoordinator},
};
use crate::agents::{
    lifecycle::{AgentLifecycleEvent, SessionLifecycleHook},
    sessions::Sessions,
};
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
    lifecycle_hooks: Vec<Box<dyn SessionLifecycleHook>>,
    subagents: CoordinatorRuntime,
}
impl SessionBundleResources {
    pub fn start(session: SessionId, sessions: Sessions) -> Self {
        let subagents = CoordinatorRuntime::start(session, sessions);
        let lifecycle_hooks: Vec<Box<dyn SessionLifecycleHook>> =
            vec![Box::new(subagents.handle())];
        Self {
            subagents,
            lifecycle_hooks,
        }
    }
    pub fn handle(&self) -> SessionBundleHandle {
        SessionBundleHandle {
            subagents: self.subagents.handle(),
        }
    }
    pub(crate) fn notify(&self, event: AgentLifecycleEvent) {
        for hook in &self.lifecycle_hooks {
            hook.notify(event.clone());
        }
    }
    pub async fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.subagents.cleanup().await
    }
}
