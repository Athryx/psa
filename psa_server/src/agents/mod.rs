//! Main agent runtime

use crate::sessions::SessionId;
use events::EventId;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub u64);

pub mod agent_state;
pub mod events;

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub parent_id: Option<AgentId>,
    pub first_event_id: EventId,
    pub next_event_id: EventId,
    pub created_position: u64,
    pub name: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub name: String,
}

/// Current state of the agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Agent is idle and has no pending inference requests.
    ///
    /// Agent becomes idle after finishing on its own accord.
    Idle,
    /// Agent was running, but was stopped due to user interrupt, api error, or quitting
    Interrupted,
    /// Agent is currently running / ready to run
    Running,
}
