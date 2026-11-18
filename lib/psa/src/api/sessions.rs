//! Sessions, projects, forks, and session listing.

use crate::api::{Agent, AgentId, events::EventId};
use llm_api::providers::{Model, ModelConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Identifier of a persisted session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub u64);

/// Client-chosen name grouping the sessions of one application or workflow.
///
/// Sessions without a project are global, which is where interactive clients
/// create sessions. API clients typically reuse the name of their
/// [`crate::api::ConfigGroup::Custom`] namespace so sessions and configurations line up.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

impl ProjectId {
    /// Maximum project ID length in bytes.
    pub const MAX_LEN: usize = 256;

    /// Reject blank IDs, surrounding whitespace, and IDs longer than [`Self::MAX_LEN`].
    pub fn validate(&self) -> Result<(), String> {
        if self.0.trim().is_empty() || self.0.trim() != self.0 {
            return Err("project ID must be nonblank without surrounding whitespace".into());
        }
        if self.0.len() > Self::MAX_LEN {
            return Err(format!(
                "project ID must be at most {} bytes",
                Self::MAX_LEN
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Session metadata and default execution environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// Source session when this session is a fork.
    pub parent_id: Option<SessionId>,
    pub name: String,
    /// Working directory on the session's probe, which may be offline.
    pub working_directory: PathBuf,
    /// Probe providing the session's execution environment.
    pub probe_id: crate::api::ProbeId,
    /// Project the session belongs to; `None` for global sessions.
    pub project_id: Option<ProjectId>,
    /// Default model configuration for agents without a pinned model.
    pub model_config: ModelConfig,
    /// Session decision model, called from CodeMode by agents whose
    /// `DecisionModels` bundle selects `Global`. `None` when unset.
    pub decision_model: Option<Model>,
    /// Whether the runtime reports active work for this session.
    pub busy: bool,
    /// Creation time in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last persisted change (metadata, agent creation, or event commit), in RFC 3339 format.
    /// Busy-only transitions do not advance this value.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Parameters for creating a root session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    pub name: String,
    /// Working directory on the selected probe.
    pub working_directory: PathBuf,
    /// Probe that provides the execution environment.
    pub probe_id: crate::api::ProbeId,
    /// Project to create the session in; omission or null creates a global session.
    /// Fixed for the session's lifetime and inherited by its forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// Model configuration for this session. Omission or null uses the server's
    /// configured session defaults; the resolved value is persisted in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_config: Option<ModelConfig>,
    /// Session decision model, if any; must be a decision model in the catalog.
    /// Only `DecisionModels` bundles selecting `Global` use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_model: Option<Model>,
}

/// Optional session metadata changes; absent fields remain unchanged.
/// The probe and project are fixed when the session is created.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSession {
    /// Replacement session name.
    pub name: Option<String>,
    /// Replacement working directory on the existing probe.
    pub working_directory: Option<PathBuf>,
    /// Replacement default model configuration.
    pub model_config: Option<ModelConfig>,
    /// Replacement session decision model. Absent leaves it unchanged; it cannot
    /// be cleared. Must be a decision model in the catalog.
    #[serde(default)]
    pub decision_model: Option<Model>,
}

/// Inclusive transcript boundary at which to fork a session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ForkPoint {
    /// Forks through the session's current head.
    Head,
    /// Forks through the session position of a visible agent event.
    ThroughEvent {
        /// Agent whose lineage contains the boundary event.
        agent_id: AgentId,
        /// Inclusive boundary event in that agent's lineage.
        event_id: EventId,
    },
}

/// Parameters for forking a session and its agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSession {
    /// Inclusive boundary for inherited history.
    pub point: ForkPoint,
    /// Overrides for the fork's metadata; defaults to no overrides.
    #[serde(default)]
    pub metadata: UpdateSession,
}

/// Created session fork and its agents, each retaining its source in `parent_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkedSession {
    /// Newly created session.
    pub session: Session,
    /// Forked agents; each `parent_id` identifies the source agent.
    pub agents: Vec<Agent>,
}

/// Session selection shared by summary pages and by clients applying live
/// summary updates, so both agree on membership. Criteria combine with AND;
/// the default filter matches every session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionFilter {
    /// Substring of the session name, compared ignoring ASCII case; empty matches all.
    pub name_contains: String,
    /// Sessions on any of these probes; empty matches every probe.
    pub probe_ids: Vec<crate::api::ProbeId>,
    /// Sessions in any of these projects, with `None` selecting global sessions;
    /// empty matches every project.
    pub project_ids: Vec<Option<ProjectId>>,
}

impl SessionFilter {
    /// Whether the filter matches every session.
    pub fn is_empty(&self) -> bool {
        self.name_contains.is_empty() && self.probe_ids.is_empty() && self.project_ids.is_empty()
    }

    /// Whether `session` satisfies every criterion, exactly as the server applies them.
    pub fn matches(&self, session: &Session) -> bool {
        let name = session
            .name
            .to_ascii_lowercase()
            .contains(&self.name_contains.to_ascii_lowercase());
        let probe = self.probe_ids.is_empty() || self.probe_ids.contains(&session.probe_id);
        let project = self.project_ids.is_empty() || self.project_ids.contains(&session.project_id);
        name && probe && project
    }
}

/// Ordered, keyset-paginated session summaries, newest update first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsQuery {
    /// Include roots and forks; incompatible with `parent`.
    pub flat: bool,
    /// Direct children of this session; absent selects roots unless `flat`.
    pub parent: Option<SessionId>,
    /// Name, probe, and project criteria.
    pub filter: SessionFilter,
    /// Opaque exclusive cursor from the previous page of this same query.
    pub before: Option<String>,
    /// Page size, 1 through 1000; defaults to 100.
    pub limit: u32,
}

impl Default for SessionsQuery {
    fn default() -> Self {
        Self {
            flat: false,
            parent: None,
            filter: SessionFilter::default(),
            before: None,
            limit: 100,
        }
    }
}

/// Session summaries captured at one publication boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsPage {
    /// Ordered by updated_at descending, then ID descending.
    pub sessions: Vec<Session>,
    /// Exclusive older-page cursor, absent at the end.
    pub next_before: Option<String>,
    /// Global publication revision at which these summaries were captured.
    pub revision: u64,
}
