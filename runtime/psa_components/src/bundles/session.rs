//! Session-scoped owners. Installed agent bundles borrow handles, never ownership.
use super::{
    CleanupError,
    decision_models::DecisionHandle,
    subagents::{CoordinatorRuntime, SubagentCoordinator},
};
use futures_util::future::BoxFuture;
use psa::api::SessionId;
use psa_agents::factory::{SessionComponents, SessionComponentsHandle};
use psa_agents::{
    lifecycle::{AgentLifecycleEvent, SessionLifecycleHook},
    runtime_handle::RuntimeHandle,
};

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
        sessions: RuntimeHandle,
        providers: Option<psa_models::ProviderRegistry>,
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

impl SessionComponents for SessionBundleResources {
    fn handle(&self) -> SessionComponentsHandle {
        SessionComponentsHandle::new(SessionBundleResources::handle(self))
    }

    fn notify(&self, event: AgentLifecycleEvent) {
        SessionBundleResources::notify(self, event);
    }

    fn cleanup(&mut self) -> BoxFuture<'_, Result<(), CleanupError>> {
        Box::pin(SessionBundleResources::cleanup(self))
    }
}
