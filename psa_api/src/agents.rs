//! Agents, their usage, and requests submitted to them.

use crate::{
    AgentDefinitionId, ConfigId, ModelSelection, SessionId,
    events::{Event, EventId, EventType},
};
use llm_api::{
    api::{ContentBlock, Usage},
    providers::{Model, ModelConfig},
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Identifier of a persisted agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub u64);

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

/// Agent metadata, ancestry, state, and cumulative usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    /// Immutable definition version pinned to this agent.
    pub definition_id: AgentDefinitionId,
    /// Session containing this agent.
    pub session_id: SessionId,
    /// Fork lineage; independent of subagent ancestry.
    pub parent_id: Option<AgentId>,
    /// Immutable subagent parent, independent of fork lineage.
    pub subagent_parent_id: Option<AgentId>,
    pub name: String,
    /// Tool and hook metadata; does not itself change runtime behavior.
    pub interactive: bool,
    /// Client visibility metadata only; the server never filters hidden agents.
    pub hidden: bool,
    /// Current persisted execution state.
    pub state: AgentState,
    /// Exact cumulative usage, including inherited inference results,
    /// compactions, and decision-call charges. A count is unknown (`None`) once
    /// any call omitted it; `provider_details` stays empty. Do not sum forks to
    /// estimate provider spend.
    pub usage: Usage,
    /// Next event sequence number to assign in this agent's lineage.
    pub next_event_id: EventId,
    /// Creation time in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last persisted update time in RFC 3339 format.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Create an agent by resolving and pinning a current named configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    /// Configuration whose current reachable graph is resolved.
    pub config_id: ConfigId,
    /// Display name for this agent instance.
    pub name: String,
    /// Optional subagent parent; omission means no parent.
    #[serde(default)]
    pub subagent_parent_id: Option<AgentId>,
    /// Metadata available to hooks and tools; defaults to false.
    #[serde(default)]
    pub interactive: bool,
    /// Client visibility metadata only; defaults to false without server filtering.
    #[serde(default)]
    pub hidden: bool,
    /// Selection that must exactly match an allowed entry, including effort policy.
    /// Omission/null selects the sole entry, or the first entry when defaults are enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
}

/// Cumulative usage of one model by an agent, including inherited calls.
/// Decision-model calls add their reported usage; counts a decision provider
/// does not report become unknown. Failed decision calls are skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model the calls were made to.
    pub model: Model,
    /// Totals over every call to this model.
    pub usage: Usage,
}

/// Per-agent model and most recent inference usage at an event boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentContextState {
    /// Inclusive agent event sequence represented by this snapshot.
    pub head: u64,
    /// Effective model in the current context; unknown before a model event.
    pub model_config: Option<ModelConfig>,
    /// Last inference total tokens, not the current context size or cumulative usage.
    pub context_tokens: Option<u64>,
}

impl AgentContextState {
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

/// Scheduling policy for a submitted agent command. Commands never start,
/// continue, or resume a run; delivery only chooses when they execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandDelivery {
    /// Waits until the agent is no longer running before executing.
    Queue,
    /// Executes at the next worker boundary, including between turns of a run.
    Steer,
}

/// A typed command handled by hooks that installed bundles register.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AgentCommand {
    /// Summarizes older context now, regardless of the automatic threshold.
    /// Requires the `Compaction` bundle.
    Compact {
        /// Extra summarization guidance, such as what to focus on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
}

impl AgentCommand {
    /// Slash-command name, without the slash.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Compact { .. } => "compact",
        }
    }

    /// Arguments as they would be typed after the name; empty when there are none.
    pub fn args(&self) -> String {
        match self {
            Self::Compact { instructions } => instructions.clone().unwrap_or_default(),
        }
    }
}

/// A typed command to run on an agent. The request completes when the command
/// has finished, and the worker records it in the agent's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCommand {
    /// When the command runs relative to the agent's current work.
    pub delivery: CommandDelivery,
    /// Command to run.
    pub command: AgentCommand,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Succeeded,
    Failed,
    /// Stopped by an interrupt before it finished.
    Interrupted,
}

/// Outcome of a finished command, with any text it reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    pub outcome: CommandOutcome,
    /// Text the command reported, such as a failure reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// An asynchronous control or message request for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum AgentRequest {
    /// Requests interruption of the current run.
    Interrupt,
    /// A message for the agent, delivered according to `delivery`.
    Message {
        /// When the message is delivered.
        delivery: MessageDelivery,
        /// Message content supplied to the agent.
        content: Vec<ContentBlock>,
        /// Sending agent, which must be in the same session. A message with a
        /// source is recorded as a subagent message under the sender's current
        /// name; without one it is a human message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<AgentId>,
    },
    /// Record a client-side slash command that has already finished. It is
    /// steered: recorded at the agent's next boundary, even during a run.
    /// Agent commands are run and recorded with [`RunCommand`] instead.
    RecordCommand(crate::events::CommandRecord),
}

/// Acknowledgment that an asynchronous agent request was accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestAccepted {
    /// Identifier used to correlate later request failures.
    pub request_id: String,
}
