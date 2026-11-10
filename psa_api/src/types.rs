//! Shared API identifiers, configuration, requests, responses, and stream messages.

use crate::events::{Event, EventId, EventType};
use crate::{BundleConfig, ClientQueryId, ClientQueryRequest, HookConfigs};
use llm_api::{api::ContentBlock, providers::ModelConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Identifier of a persisted session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(
    /// Underlying numeric identifier.
    pub u64,
);

/// Identifier of a persisted agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(
    /// Underlying numeric identifier.
    pub u64,
);

/// Identifier of an immutable agent definition version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentDefinitionId(
    /// Underlying numeric identifier.
    pub u64,
);

/// Identity of a built-in lifecycle hook, without its construction data.
/// Use [`crate::HookConfig`] when selecting and configuring an installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookId {
    /// Performs no work.
    Noop,
    /// Stops after a response without tool calls, unless installed goal policy applies.
    StopWithoutTools,
    /// Initializes a root agent's model and system context.
    DefaultStartup,
    /// Appends the system prompt supplied by its hook configuration.
    ExtraSystemPrompt,
    /// Summarizes older context before the next inference.
    Compaction,
    /// Loads project instructions from the working directory hierarchy.
    ProjectContext,
    /// Publishes the installed tool definitions.
    ToolDefinitions,
    /// Adds the session working directory to context.
    Cwd,
    /// Loads the definition's configured skills.
    Skills,
    /// Follows session model changes unless the definition pins a model.
    SessionModelUpdate,
}

/// Identity of a built-in feature bundle, without its construction data.
/// Use [`BundleConfig`] when selecting a bundle installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BundleId {
    /// Installs a tool that performs no work.
    Noop,
    /// Installs Read and, unless read-only, Edit and Write.
    FileTools,
    /// Installs the Bash, InteractBash, and CloseBash tools with shared sessions.
    Bash,
    /// Installs the CodeMode execution and reset tools.
    CodeMode,
    /// Installs goal tools and a goal-aware stopping policy.
    Goal,
    /// Installs the tool for requesting structured client input.
    AskQuestion,
    /// Installs session-wide agent tools backed by core ancestry and steering.
    Subagents,
    /// Installs schema-validated result recording and its completion policy.
    TypedResult,
}

/// An immutable, versioned agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Identifier of this definition version.
    pub id: AgentDefinitionId,
    /// Previous immutable version, if this definition replaces one.
    pub previous_version_id: Option<AgentDefinitionId>,
    /// Version number within the definition's version chain.
    pub version: u64,
    /// Definition name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this definition is offered for selection.
    pub selectable: bool,
    /// Ordered feature bundle construction configurations.
    #[serde(default)]
    pub bundles: Vec<BundleConfig>,
    /// Ordered hook construction configurations for each lifecycle stage.
    pub hooks: HookConfigs,
    /// Pinned model configuration; absent to follow the session model.
    pub model_config: Option<ModelConfig>,
    /// Paths to skill files or directories.
    pub skills: Vec<PathBuf>,
    /// Definition versions this agent may use for subagents.
    pub permitted_subagents: Vec<AgentDefinitionId>,
}

/// Configuration for creating an immutable agent definition version.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentDefinition {
    /// Definition name.
    pub name: String,
    /// Human-readable description; defaults to empty.
    #[serde(default)]
    pub description: String,
    /// Whether this definition is offered for selection; defaults to false.
    #[serde(default)]
    pub selectable: bool,
    /// Ordered feature bundle configurations; defaults to empty.
    #[serde(default)]
    pub bundles: Vec<BundleConfig>,
    /// Ordered lifecycle hook configurations; defaults to no hooks.
    #[serde(default)]
    pub hooks: HookConfigs,
    /// Pinned model configuration; absent to follow the session model.
    pub model_config: Option<ModelConfig>,
    /// Paths to skill files or directories; defaults to empty.
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    /// Allowed subagent definition versions; defaults to empty.
    #[serde(default)]
    pub permitted_subagents: Vec<AgentDefinitionId>,
}

/// A complete replacement that creates a new immutable definition version.
pub type UpdateAgentDefinition = CreateAgentDefinition;

/// Persisted execution state of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Not currently running.
    Idle,
    /// Stopped by interruption or an unsuccessful inference.
    Interrupted,
    /// Actively executing an agent run.
    Running,
}

/// Identity of an installed native agent tool, independent of construction data.
/// Tools are installed through their owning [`BundleConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolId {
    /// Performs no work.
    Noop,
    /// Reads file contents.
    Read,
    /// Edits a file using exact text replacements.
    Edit,
    /// Writes file contents.
    Write,
    /// Starts a shell command.
    Bash,
    /// Sends input to or polls a running shell command.
    InteractBash,
    /// Stops a running shell command.
    CloseBash,
    /// Executes code through the CodeMode tool.
    CodeMode,
    /// Resets the CodeMode REPL without deleting prior checkpoints or tool results.
    ResetCodeMode,
    /// Sets the agent's goal when goal mode is installed.
    SetGoal,
    /// Updates the status of the agent's goal.
    SetGoalStatus,
    /// Requests structured input from a connected client.
    AskQuestion,
    /// Creates a subagent from a permitted definition.
    CreateAgent,
    /// Sends an attributed message to another agent.
    SendMessage,
    /// Waits for an agent message.
    WaitMessage,
    /// Lists agents visible to the subagent tools.
    ListAgents,
    /// Records a structured result validated against the installed result schema.
    RecordResult,
}

/// Content blocks returned by a native tool.
pub type ToolResult = Vec<ContentBlock>;

/// Exact cumulative counts. A field is unknown if any call omitted it.
/// Includes inherited calls; do not sum forks to estimate provider spend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsage {
    /// Cumulative input tokens, or unknown.
    pub input_tokens: Option<u64>,
    /// Cumulative output tokens, or unknown.
    pub output_tokens: Option<u64>,
    /// Cumulative total tokens, or unknown.
    pub total_tokens: Option<u64>,
    /// Cumulative cached input tokens, or unknown.
    pub cached_input_tokens: Option<u64>,
    /// Cumulative input tokens written to provider caches, or unknown.
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    /// Cumulative reasoning output tokens, or unknown.
    pub reasoning_output_tokens: Option<u64>,
}

impl Default for AgentUsage {
    fn default() -> Self {
        Self {
            input_tokens: Some(0),
            output_tokens: Some(0),
            total_tokens: Some(0),
            cached_input_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_output_tokens: Some(0),
        }
    }
}

/// Per-agent model and most recent inference usage at an event boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStatusSnapshot {
    /// Inclusive agent event sequence represented by this snapshot.
    pub head: u64,
    /// Effective model in the current context; unknown before a model event.
    pub model_config: Option<ModelConfig>,
    /// Last inference total tokens, not the current context size or cumulative usage.
    pub context_tokens: Option<u64>,
}

impl AgentStatusSnapshot {
    /// Incorporate a newer committed event. Older replay and history are ignored.
    pub fn apply_event(&mut self, event: &Event) {
        if event.id.0 <= self.head {
            return;
        }
        self.head = event.id.0;
        match &event.kind {
            EventType::NewContext => {
                self.model_config = None;
                self.context_tokens = None;
            }
            EventType::Model(config) => self.model_config = Some(config.clone()),
            EventType::Compaction(_) => self.context_tokens = None,
            EventType::InferenceResultEnd(end) => {
                self.context_tokens = if matches!(
                    end.outcome,
                    llm_api::api::Outcome::Completed { .. }
                        | llm_api::api::Outcome::Incomplete { .. }
                ) {
                    end.usage.total_tokens.or_else(|| {
                        Some(
                            end.usage
                                .input_tokens?
                                .saturating_add(end.usage.output_tokens?),
                        )
                    })
                } else {
                    None
                };
            }
            _ => {}
        }
    }
}

/// Session metadata and default execution environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session identifier.
    pub id: SessionId,
    /// Source session when this session is a fork.
    pub parent_id: Option<SessionId>,
    /// Session name.
    pub name: String,
    /// Working directory on the session's probe, which may be offline.
    pub working_directory: PathBuf,
    /// Probe providing the session's execution environment.
    pub probe_id: crate::ProbeId,
    /// Default model configuration for agents without a pinned model.
    pub model_config: ModelConfig,
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

/// Agent metadata, ancestry, state, and cumulative usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Agent identifier.
    pub id: AgentId,
    /// Immutable definition version pinned to this agent.
    pub definition_id: AgentDefinitionId,
    /// Session containing this agent.
    pub session_id: SessionId,
    /// Fork lineage; independent of subagent ancestry.
    pub parent_id: Option<AgentId>,
    /// Immutable subagent parent, independent of fork lineage.
    #[serde(default)]
    pub subagent_parent_id: Option<AgentId>,
    /// Agent name.
    pub name: String,
    /// Tool and hook metadata; does not itself change runtime behavior.
    pub interactive: bool,
    /// Client visibility metadata only; the server never filters hidden agents.
    #[serde(default)]
    pub hidden: bool,
    /// Current persisted execution state.
    pub state: AgentState,
    /// Cumulative usage, including inherited inference results.
    pub usage: AgentUsage,
    /// Next event sequence number to assign in this agent's lineage.
    pub next_event_id: EventId,
    /// Creation time in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last persisted update time in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Parameters for creating a root session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    /// Session name.
    pub name: String,
    /// Working directory on the selected probe.
    pub working_directory: PathBuf,
    /// Probe that provides the execution environment.
    pub probe_id: crate::ProbeId,
    /// Model configuration for this session. Omission or null uses the server's
    /// configured session defaults; the resolved value is persisted in the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_config: Option<ModelConfig>,
}

/// Optional session metadata changes; absent fields remain unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSession {
    /// Replacement session name.
    pub name: Option<String>,
    /// Replacement working directory on the existing probe.
    pub working_directory: Option<PathBuf>,
    /// Replacement default model configuration.
    pub model_config: Option<ModelConfig>,
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

/// Parameters for creating an agent in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    /// Immutable parent in the same session, established before agent startup.
    #[serde(default)]
    pub subagent_parent_id: Option<AgentId>,
    /// Immutable definition version to pin.
    pub definition_id: AgentDefinitionId,
    /// Agent name.
    pub name: String,
    /// Metadata for tools and hooks; does not change runtime behavior.
    #[serde(default)]
    pub interactive: bool,
    /// Client visibility metadata only; defaults to false without server filtering.
    #[serde(default)]
    pub hidden: bool,
}

/// Sender attribution for a subagent message. Names are snapshots at submission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageSource {
    /// Sending agent identifier, if known.
    pub agent_id: Option<AgentId>,
    /// Sending agent name captured at submission, if known.
    pub name: Option<String>,
}

/// Scheduling policy for a submitted agent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDelivery {
    /// Waits until the agent is no longer running before delivering.
    Queue,
    /// Delivers at the next worker boundary, including during a run.
    Steer,
    /// Delivers only to an interrupted agent and resumes it.
    Resume,
}

/// An asynchronous control or message request for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AgentRequest {
    /// Requests interruption of the current run.
    Interrupt,
    /// An attributed message delivered using ordinary steering semantics.
    SubagentMessage {
        /// Attributed message content.
        content: Vec<ContentBlock>,
        /// Optional sender attribution captured at submission.
        #[serde(default)]
        source: Option<AgentMessageSource>,
    },
    /// A message with an explicit delivery policy.
    Message {
        /// Policy controlling when this message is delivered.
        delivery: MessageDelivery,
        /// Message content supplied to the agent.
        content: Vec<ContentBlock>,
    },
}

/// Acknowledgment that an asynchronous agent request was accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestAccepted {
    /// Identifier used to correlate later request failures.
    pub request_id: String,
}

/// Filter for listing direct child sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSessions {
    /// Parent session to list children of; absent to list root sessions only.
    pub parent: Option<SessionId>,
}

/// Ordered, keyset-paginated session summaries. Legacy listing remains unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionListQuery {
    /// Include roots and forks; incompatible with parent.
    pub flat: bool,
    /// Direct children of this session; absent selects roots unless flat.
    pub parent: Option<SessionId>,
    /// Restrict to one probe.
    pub probe_id: Option<crate::ProbeId>,
    /// Opaque exclusive cursor returned by the previous page.
    pub before: Option<String>,
    /// Page size, 1 through 1000.
    pub limit: u32,
}
impl Default for SessionListQuery {
    fn default() -> Self {
        Self {
            flat: false,
            parent: None,
            probe_id: None,
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

/// Typed allowlist of supported global data events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobalEventKind {
    /// Full session summary upserts.
    #[default]
    SessionUpdated,
}

/// Global summary subscription selection, independent of transcript routes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalEventsQuery {
    /// Selected data-event kind.
    pub kind: GlobalEventKind,
    /// Restrict summaries to one probe.
    pub probe_id: Option<crate::ProbeId>,
}

/// Ephemeral global feed protocol. Reconnects require a new summary bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GlobalStreamMessage {
    /// Subscription established; fetch pages only after this boundary.
    Ready {
        /// Current global publication revision.
        revision: u64,
    },
    /// Full committed session summary, ordered by revision.
    SessionUpdated {
        /// Monotonic publication revision within this server lifetime.
        revision: u64,
        /// Session metadata and published busy state.
        session: Session,
    },
    /// Buffered updates were lost, or a reconnect attempted ephemeral replay.
    ResyncRequired,
}

/// Pagination and provisional-event options shared by event pages and streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsQuery {
    /// Exclusive EventId for an agent route; session position for a session route.
    pub after: u64,
    /// Fetch the latest committed page; JSON only, incompatible with forward bounds.
    pub recent: bool,
    /// Exclusive older-history cursor; JSON only, incompatible with forward bounds/recent.
    pub before: Option<u64>,
    /// Inclusive upper cursor bound; absent to use the current head.
    pub through: Option<u64>,
    /// Maximum page size, from 1 through 10,000; defaults to 1,000.
    pub limit: u32,
    /// Whether to include provisional events; defaults to false.
    pub include_pending: bool,
    /// Restrict event payloads to durable result artifacts; defaults to false.
    /// Cursors still refer to the full event sequence, not a separate result sequence.
    #[serde(default)]
    pub results_only: bool,
    /// Begin at the model context containing `through`: the page starts with the
    /// NewContext event that opened it, or else the agent's first event. Agent
    /// routes only; JSON only; requires `through` and excludes every other lower
    /// bound. Continue a partial page with `after = next_after` and this flag unset.
    #[serde(default)]
    pub from_context_start: bool,
}

impl Default for EventsQuery {
    fn default() -> Self {
        Self {
            after: 0,
            recent: false,
            before: None,
            through: None,
            limit: 1000,
            include_pending: false,
            results_only: false,
            from_context_start: false,
        }
    }
}

/// A transcript event located in a session's view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Session in which this event is viewed.
    pub session_id: SessionId,
    /// Agent as viewed in this session, distinct from event.source_agent_id.
    pub agent_id: AgentId,
    /// Session-wide event position used by session event cursors.
    pub position: u64,
    /// Immutable transcript event.
    pub event: Event,
}

/// A provisional transcript item that may be updated, committed, or removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEvent {
    /// Identifier of this provisional item.
    pub id: String,
    /// Revision of this provisional item's contents.
    pub revision: u64,
    /// Session containing the provisional item.
    pub session_id: SessionId,
    /// Agent publishing the provisional item.
    pub agent_id: AgentId,
    /// Current provisional event payload.
    pub kind: EventType,
}

/// A transcript page with live provisional state and pagination cursors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsPage {
    /// Committed events in cursor order.
    pub events: Vec<EventEnvelope>,
    /// Current provisional items when requested.
    pub pending: Vec<PendingEvent>,
    /// Outstanding client queries for the requested scope.
    #[serde(default)]
    pub queries: Vec<ClientQueryRequest>,
    /// Exclusive cursor to use for the next page.
    pub next_after: u64,
    /// Exclusive cursor for older history, absent when no older events remain.
    #[serde(default)]
    pub next_before: Option<u64>,
    /// Whether older committed events exist before this page.
    #[serde(default)]
    pub has_older: bool,
    /// Captured head cursor, clamped to the requested upper bound.
    pub head: u64,
    /// Whether additional committed events remain before the captured head.
    pub has_more: bool,
}

/// A server-sent notification for transcript or runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// Marks the end of historical replay and supplies current live state.
    Snapshot {
        /// Captured committed head cursor.
        cursor: u64,
        /// Current provisional items, when requested.
        pending: Vec<PendingEvent>,
        /// Outstanding client queries.
        #[serde(default)]
        queries: Vec<ClientQueryRequest>,
    },
    /// Publishes a client query awaiting a response.
    ClientQueryRequested {
        /// Query that clients may answer.
        query: ClientQueryRequest,
    },
    /// Removes a client query that is no longer outstanding.
    ClientQueryRemoved {
        /// Session containing the query.
        session_id: SessionId,
        /// Agent that issued the query.
        agent_id: AgentId,
        /// Identifier of the removed query.
        query_id: ClientQueryId,
    },
    /// Creates or replaces a provisional event.
    Upsert {
        /// Latest provisional event revision.
        pending: PendingEvent,
    },
    /// Publishes a committed transcript event.
    Committed {
        /// Committed event and its session location.
        event: EventEnvelope,
        /// Provisional identifier replaced by this commit, if any.
        pending_id: Option<String>,
    },
    /// Removes a provisional event without committing it.
    Removed {
        /// Session containing the provisional item.
        session_id: SessionId,
        /// Agent that published the provisional item.
        agent_id: AgentId,
        /// Identifier of the removed provisional item.
        id: String,
    },
    /// Publishes refreshed agent metadata.
    AgentUpdated {
        /// Updated agent snapshot.
        agent: Agent,
    },
    /// Publishes a full snapshot after session metadata, committed history, agent creation, or activity changes.
    /// Apply snapshots in stream order; activity changes do not advance `updated_at`.
    SessionUpdated {
        /// Updated session, including its current published busy state.
        session: Session,
    },
    /// Reports failure of an accepted asynchronous agent request.
    RequestFailed {
        /// Session containing the target agent.
        session_id: SessionId,
        /// Target agent identifier.
        agent_id: AgentId,
        /// Identifier returned when the request was accepted.
        request_id: String,
        /// Human-readable failure description.
        message: String,
    },
    /// Signals that the client must reload state after replay failure or stream lag.
    ResyncRequired,
}

/// Structured error returned by an API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// Machine-readable error code.
    pub code: String,
    /// Human-readable error description.
    pub message: String,
}
