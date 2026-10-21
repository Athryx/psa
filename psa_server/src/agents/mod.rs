//! Main agent runtime

use crate::{agents::agent_projection::ProjectionError, db::DbError, sessions::SessionId};
use psa_api::events::{EventId, ToolDefinition};
use time::OffsetDateTime;

pub use psa_api::{AgentDefinition, AgentDefinitionId, AgentId, AgentState, AgentUsage};

pub mod agent_context;
pub mod agent_manager;
pub mod agent_projection;
pub mod publication;
pub mod session_manager;
pub mod sessions;

/// Server-only convenience for taking a tool definition snapshot.
pub trait ToolDefinitionExt {
    fn for_tool(id: crate::hooks::tools::ToolId) -> Self;
}

impl ToolDefinitionExt for ToolDefinition {
    fn for_tool(id: crate::hooks::tools::ToolId) -> Self {
        Self {
            id,
            definition: crate::hooks::tools::tool_definition(id),
        }
    }
}

/// Represents the state of the agent in the database, mostly immutable except for state
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub definition_id: AgentDefinitionId,
    pub parent_id: Option<AgentId>,
    pub first_event_id: EventId,
    pub next_event_id: EventId,
    pub created_position: u64,
    pub name: String,
    pub interactive: bool,
    pub state: AgentState,
    pub usage: AgentUsage,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl From<Agent> for psa_api::Agent {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            session_id: agent.session_id,
            definition_id: agent.definition_id,
            parent_id: agent.parent_id,
            name: agent.name,
            interactive: agent.interactive,
            state: agent.state,
            usage: agent.usage,
            next_event_id: agent.next_event_id,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub definition_id: AgentDefinitionId,
    pub name: String,
    pub interactive: bool,
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
    #[error("agent worker supervisor is unavailable")]
    SupervisorUnavailable,
}
