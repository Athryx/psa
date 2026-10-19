use crate::agents::{Agent, AgentId, events::EventId};
use std::path::PathBuf;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub first_position: u64,
    pub next_position: u64,
    pub name: String,
    pub working_directory: PathBuf,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct NewSession {
    pub name: String,
    pub working_directory: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct SessionUpdate {
    pub name: Option<String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub enum ForkPoint {
    Head,
    /// `agent_id` identifies the stream as viewed in the source session,
    /// even when the selected event is physically owned by an ancestor.
    ThroughEvent {
        agent_id: AgentId,
        event_id: EventId,
    },
}

#[derive(Debug, Clone)]
pub struct SessionFork {
    pub point: ForkPoint,
    pub metadata: SessionUpdate,
}

#[derive(Debug, Clone)]
pub struct ForkedSession {
    pub session: Session,
    pub agents: Vec<(AgentId, Agent)>,
}
