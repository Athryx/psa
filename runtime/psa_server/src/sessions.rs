use crate::agents::{Agent, AgentId};
use psa::api::events::EventId;
use std::path::PathBuf;
use time::OffsetDateTime;

pub use psa::api::SessionId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub parent_id: Option<SessionId>,
    pub first_position: u64,
    pub next_position: u64,
    pub name: String,
    pub working_directory: PathBuf,
    pub probe_id: psa::api::ProbeId,
    pub project_id: Option<psa::api::ProjectId>,
    pub model_config: llm_api::providers::ModelConfig,
    pub decision_model: Option<llm_api::providers::Model>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Session {
    pub fn into_api(self, busy: bool) -> psa::api::Session {
        psa::api::Session {
            id: self.id,
            parent_id: self.parent_id,
            name: self.name,
            working_directory: self.working_directory,
            probe_id: self.probe_id,
            project_id: self.project_id,
            model_config: self.model_config,
            decision_model: self.decision_model,
            busy,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewSession {
    pub name: String,
    pub working_directory: PathBuf,
    pub probe_id: psa::api::ProbeId,
    pub project_id: Option<psa::api::ProjectId>,
    pub model_config: llm_api::providers::ModelConfig,
    pub decision_model: Option<llm_api::providers::Model>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionUpdate {
    pub model_config: Option<llm_api::providers::ModelConfig>,
    pub decision_model: Option<llm_api::providers::Model>,
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
    pub agents: Vec<Agent>,
}
