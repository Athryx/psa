//! Session-scoped owners. Installed agent bundles borrow handles, never ownership.
use super::{
    CleanupError,
    decision_models::DecisionHandle,
    subagents::{CoordinatorRuntime, SubagentCoordinator},
};
use crate::agents::{
    lifecycle::{AgentLifecycleEvent, SessionLifecycleHook},
    sessions::Sessions,
};
use psa::api::SessionId;

#[derive(Clone)]
pub struct SessionBundleHandle {
    subagents: SubagentCoordinator,
    decisions: DecisionHandle,
}
impl SessionBundleHandle {
    pub fn subagents(&self) -> SubagentCoordinator {
        self.subagents.clone()
    }
    pub(crate) fn decisions(&self) -> DecisionHandle {
        self.decisions.clone()
    }
}

pub struct SessionBundleResources {
    lifecycle_hooks: Vec<Box<dyn SessionLifecycleHook>>,
    subagents: CoordinatorRuntime,
    decisions: DecisionHandle,
}
impl SessionBundleResources {
    /// Without a provider registry, decision calls fail with a configuration error.
    pub fn start(
        session: SessionId,
        sessions: Sessions,
        providers: Option<crate::providers::ProviderRegistry>,
    ) -> Self {
        let decisions = DecisionHandle::new(providers, sessions.model_catalog().cloned());
        let subagents = CoordinatorRuntime::start(session, sessions);
        let lifecycle_hooks: Vec<Box<dyn SessionLifecycleHook>> =
            vec![Box::new(subagents.handle())];
        Self {
            subagents,
            lifecycle_hooks,
            decisions,
        }
    }
    pub fn handle(&self) -> SessionBundleHandle {
        SessionBundleHandle {
            subagents: self.subagents.handle(),
            decisions: self.decisions.clone(),
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
