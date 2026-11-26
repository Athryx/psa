//! The seam through which the agent runtime installs components it does not
//! define. The runtime owns installed components and their lifecycle; the
//! factory decides which concrete bundles a definition selects.

use crate::{
    Agent, AgentDefinition,
    bundles::{CleanupError, InstallError, InstalledComponents},
    lifecycle::AgentLifecycleEvent,
    runtime_handle::RuntimeHandle,
};
use futures_util::future::BoxFuture;
use psa::api::{AgentCommand, BundleConfig, SessionId};
use psa_store::Session;
use std::{any::Any, sync::Arc};

pub trait ComponentFactory: Send + Sync + 'static {
    /// Reject a bundle selection before it is saved.
    fn validate_bundles(&self, bundles: &[BundleConfig]) -> Result<(), String>;

    /// Reject a command at submission unless the agent installs its handler.
    fn require_command_support(
        &self,
        definition: &AgentDefinition,
        interactive: bool,
        command: &AgentCommand,
    ) -> Result<(), String>;

    /// Whether installing `definition` needs the session's shared components.
    /// `parent` is the definition of the agent's subagent parent, if any.
    fn needs_session_components(
        &self,
        definition: &AgentDefinition,
        parent: Option<&AgentDefinition>,
    ) -> bool;

    /// Start the components shared by every agent in a session.
    fn start_session(
        &self,
        session: SessionId,
        runtime: RuntimeHandle,
        providers: psa_models::ProviderRegistry,
    ) -> Box<dyn SessionComponents>;

    /// Construct and install the bundles a definition selects.
    fn install<'a>(
        &'a self,
        definition: &'a AgentDefinition,
        agent: &'a Agent,
        session: &'a Session,
        resources: Option<&'a SessionComponentsHandle>,
    ) -> BoxFuture<'a, Result<InstalledComponents, InstallError>>;
}

/// Session-scoped owners, started by a [`ComponentFactory`]. Installed agent
/// bundles borrow their handle, never ownership.
pub trait SessionComponents: Send + Sync + 'static {
    fn handle(&self) -> SessionComponentsHandle;
    fn notify(&self, event: AgentLifecycleEvent);
    fn cleanup(&mut self) -> BoxFuture<'_, Result<(), CleanupError>>;
}

/// A cloneable, type-erased handle to session components; the factory that
/// started them downcasts it back.
#[derive(Clone)]
pub struct SessionComponentsHandle(Arc<dyn Any + Send + Sync>);

impl SessionComponentsHandle {
    pub fn new<T: Any + Send + Sync>(handle: T) -> Self {
        Self(Arc::new(handle))
    }

    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}
