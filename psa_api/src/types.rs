use crate::events::{Event, EventId, EventType};
use llm_api::{api::ContentBlock, providers::ModelConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Interrupted,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolId {
    Noop,
}

pub type ToolResult = Vec<ContentBlock>;

/// Exact cumulative counts. A field is unknown if any call omitted it.
/// Includes inherited calls; do not sum forks to estimate provider spend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
}

impl Default for AgentUsage {
    fn default() -> Self {
        Self {
            input_tokens: Some(0),
            output_tokens: Some(0),
            total_tokens: Some(0),
            cached_input_tokens: Some(0),
            reasoning_output_tokens: Some(0),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub name: String,
    pub working_directory: PathBuf,
    pub probe_id: crate::ProbeId,
    pub busy: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub parent_id: Option<AgentId>,
    pub name: String,
    pub state: AgentState,
    pub usage: AgentUsage,
    pub next_event_id: EventId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    pub name: String,
    pub working_directory: PathBuf,
    pub probe_id: crate::ProbeId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSession {
    pub name: Option<String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ForkPoint {
    Head,
    ThroughEvent {
        agent_id: AgentId,
        event_id: EventId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSession {
    pub point: ForkPoint,
    #[serde(default)]
    pub metadata: UpdateSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkedSession {
    pub session: Session,
    pub agents: Vec<(AgentId, Agent)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    pub name: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub tools: Vec<ToolId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDelivery {
    Queue,
    Steer,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AgentRequest {
    Interrupt,
    Message {
        delivery: MessageDelivery,
        content: Vec<ContentBlock>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestAccepted {
    pub request_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSessions {
    pub parent: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsQuery {
    /// Exclusive EventId for an agent route; session position for a session route.
    pub after: u64,
    pub through: Option<u64>,
    pub limit: u32,
    pub include_pending: bool,
    pub stream: bool,
}

impl Default for EventsQuery {
    fn default() -> Self {
        Self {
            after: 0,
            through: None,
            limit: 1000,
            include_pending: false,
            stream: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub session_id: SessionId,
    /// Agent as viewed in this session, distinct from event.source_agent_id.
    pub agent_id: AgentId,
    pub position: u64,
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEvent {
    pub id: String,
    pub revision: u64,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub kind: EventType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<EventEnvelope>,
    pub pending: Vec<PendingEvent>,
    pub next_after: u64,
    pub head: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    Snapshot {
        cursor: u64,
        pending: Vec<PendingEvent>,
    },
    Upsert {
        pending: PendingEvent,
    },
    Committed {
        event: EventEnvelope,
        pending_id: Option<String>,
    },
    Removed {
        session_id: SessionId,
        agent_id: AgentId,
        id: String,
    },
    AgentUpdated {
        agent: Agent,
    },
    SessionBusy {
        session_id: SessionId,
        busy: bool,
    },
    SessionUpdated {
        session: Session,
    },
    RequestFailed {
        session_id: SessionId,
        agent_id: AgentId,
        request_id: String,
        message: String,
    },
    ResyncRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}
