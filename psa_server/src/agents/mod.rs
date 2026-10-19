//! Main agent runtime

use crate::{agents::agent_context::ProjectionError, db::DbError, sessions::SessionId};
use events::EventId;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub u64);

pub mod agent_context;
pub mod agent_manager;
pub mod events;
pub mod session_manager;

pub use agent_manager::AgentState;

/// Represents the state of the agent in the database, mostly immutable except for state
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub parent_id: Option<AgentId>,
    pub first_event_id: EventId,
    pub next_event_id: EventId,
    pub created_position: u64,
    pub name: String,
    pub state: AgentState,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Error converting event state into model context: {0}")]
    Context(#[from] ProjectionError),
    #[error("Database Error: {0}")]
    Database(#[from] DbError),
}
