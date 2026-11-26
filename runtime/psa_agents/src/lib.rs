//! The agent runtime: the supervisor, per-agent workers, the agent loop, and
//! the interfaces components (bundles, tools, hooks, codemode functions)
//! implement. Concrete components live in `psa_components`; the runtime
//! installs them through a [`factory::ComponentFactory`].

use crate::agent_projection::ProjectionError;
use psa_store::DbError;

pub use psa::api::{AgentDefinition, AgentDefinitionId, AgentId, AgentState};
pub use psa_store::{Agent, NewAgent};

pub mod agent_context;
pub mod agent_manager;
pub mod agent_projection;
pub mod bundles;
pub mod cancellation;
pub mod client_queries;
pub mod codemode_functions;
pub mod config_discovery;
pub mod factory;
pub mod hooks;
pub mod lifecycle;
pub mod probes;
pub mod prompts;
pub mod publication;
pub mod runtime_handle;
pub mod supervisor;
pub mod tools;

/// Dependencies the exported macros expand to; not part of the API.
#[doc(hidden)]
pub mod __private {
    pub use futures_util::future::BoxFuture;
    pub use llm_api;
    pub use serde_json;
}

/// Store fixtures shared by unit tests.
#[cfg(any(test, feature = "testing"))]
pub mod test_support {
    pub use psa_store::test_support::{
        create_agent_definition, model_config, probe_metadata, register_probe,
    };
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Error converting event state into model context: {0}")]
    Context(#[from] ProjectionError),
    #[error("Database Error: {0}")]
    Database(#[from] DbError),
    #[error("invalid inference request: {0}")]
    Request(#[from] agent_projection::RequestError),
    #[error("model request failed: {0}")]
    Model(#[from] llm_api::Error),
    #[error("hook failed: {0}")]
    Hook(Box<crate::hooks::HookError>),
    #[error("hook cancellation timed out")]
    HookTimeout,
    #[error("event persistence task failed: {0}")]
    PersistenceTask(tokio::task::JoinError),
    #[error("bundle installation failed: {0}")]
    BundleInstall(#[from] crate::bundles::InstallError),
    #[error("bundle cleanup failed: {0}")]
    BundleCleanup(#[from] crate::bundles::CleanupErrors),
    #[error("agent worker supervisor is unavailable")]
    SupervisorUnavailable,
}
