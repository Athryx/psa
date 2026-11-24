//! Main agent runtime

use crate::agents::agent_projection::ProjectionError;
use psa::api::events::ToolDefinition;
use psa_store::DbError;

pub use psa::api::{AgentDefinition, AgentDefinitionId, AgentId, AgentState};
pub use psa_store::{Agent, NewAgent};

pub mod agent_context;
pub mod agent_manager;
pub mod agent_projection;
pub mod client_queries;
pub mod lifecycle;
pub mod publication;
pub mod runtime_handle;
pub mod supervisor;

/// Server-only convenience for taking a tool definition snapshot.
pub trait ToolDefinitionExt: Sized {
    fn for_tool(id: crate::components::tools::ToolId) -> Option<Self>;
}

impl ToolDefinitionExt for ToolDefinition {
    fn for_tool(id: crate::components::tools::ToolId) -> Option<Self> {
        Some(Self {
            id,
            definition: crate::components::tools::tool_definition(id)?,
        })
    }
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
    Hook(Box<crate::components::hooks::HookError>),
    #[error("hook cancellation timed out")]
    HookTimeout,
    #[error("event persistence task failed: {0}")]
    PersistenceTask(tokio::task::JoinError),
    #[error("bundle installation failed: {0}")]
    BundleInstall(#[from] crate::components::bundles::InstallError),
    #[error("bundle cleanup failed: {0}")]
    BundleCleanup(#[from] crate::components::bundles::CleanupErrors),
    #[error("agent worker supervisor is unavailable")]
    SupervisorUnavailable,
}
