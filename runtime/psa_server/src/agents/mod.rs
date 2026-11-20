//! Main agent runtime

use crate::{agents::agent_projection::ProjectionError, db::DbError, sessions::SessionId};
use llm_api::api::Usage;
use psa::api::events::{EventId, ToolDefinition};
use time::OffsetDateTime;

pub use psa::api::{AgentDefinition, AgentDefinitionId, AgentId, AgentState};

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

/// Represents the state of the agent in the database, mostly immutable except for state
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub definition_id: AgentDefinitionId,
    pub forked_from: Option<AgentId>,
    pub subagent_parent_id: Option<AgentId>,
    pub first_event_id: EventId,
    pub next_event_id: EventId,
    pub created_position: u64,
    pub name: String,
    pub interactive: bool,
    pub hidden: bool,
    pub state: AgentState,
    pub usage: Usage,
    pub cache: crate::db::CacheTracking,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl From<Agent> for psa::api::Agent {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            session_id: agent.session_id,
            definition_id: agent.definition_id,
            forked_from: agent.forked_from,
            subagent_parent_id: agent.subagent_parent_id,
            name: agent.name,
            interactive: agent.interactive,
            hidden: agent.hidden,
            state: agent.state,
            usage: agent.usage,
            cacheable_input_tokens: agent.cache.cacheable_input_tokens,
            next_event_id: agent.next_event_id,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub subagent_parent_id: Option<AgentId>,
    pub definition_id: AgentDefinitionId,
    pub name: String,
    pub interactive: bool,
    pub hidden: bool,
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
