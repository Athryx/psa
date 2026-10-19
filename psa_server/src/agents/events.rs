//! Immutable transcript events. Sequence numbers are local to an agent lineage.

use crate::{agents::AgentId, agents::AgentStatus, tools::ToolId};
use llm_api::api::{
    HistoryItem, HistoryItemMetadata, Outcome, ProviderMetadata, Reasoning, RequestConfig,
    ToolDefinition as ApiToolDefinition, Usage,
};
use llm_api::providers::ModelConfig;
use std::time::Duration;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EventId(pub u64);

#[derive(Debug, Clone)]
pub struct Event {
    pub id: EventId,
    /// Physical owner, which may be an ancestor of the requested agent.
    pub source_agent_id: AgentId,
    pub timestamp: OffsetDateTime,
    pub kind: EventType,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub timestamp: OffsetDateTime,
    pub kind: EventType,
}

impl NewEvent {
    pub fn new(kind: EventType) -> Self {
        Self {
            timestamp: OffsetDateTime::now_utc(),
            kind,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventRange {
    pub after: EventId,
    pub through: Option<EventId>,
    pub limit: u32,
}

impl Default for EventRange {
    fn default() -> Self {
        Self {
            after: EventId(0),
            through: None,
            limit: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum EventType {
    StatusChange(StatusChange),
    Model(ModelConfig),
    NewContext,
    Message(Message),
    Thinking(Thinking),
    ProviderItem(HistoryItem),
    ToolDefinition(ToolDefinition),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Compaction(Compaction),
    Log(Log),
    InferenceResultStart(InferenceResultStart),
    InferenceResultEnd(InferenceResultEnd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Human,
    Agent,
    Harness,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDelivery {
    Queue,
    Steer,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub message_type: MessageType,
    pub delivery: Option<MessageDelivery>,
    pub contents: String,
    pub metadata: HistoryItemMetadata,
    /// Original rich content (citations, refusals, attachments) when available.
    /// `contents` remains the display text; plain messages leave this unset.
    pub replay_content: Option<Vec<llm_api::api::ContentBlock>>,
}

impl Message {
    pub fn text(message_type: MessageType, contents: impl Into<String>) -> Self {
        Self {
            message_type,
            contents: contents.into(),
            delivery: None,
            metadata: HistoryItemMetadata::default(),
            replay_content: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Thinking {
    pub contents: Reasoning,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub definition: ApiToolDefinition,
}

impl ToolDefinition {
    pub fn for_tool(id: ToolId) -> Self {
        Self {
            id,
            definition: id.tool_definition(),
        }
    }
}

pub type ToolCallId = String;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// Preserve unparsed arguments, including malformed JSON.
    pub arguments: String,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub content: crate::tools::ToolResult,
    pub error: bool,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone)]
pub enum CompactionSummary {
    Text(String),
    ProviderItem(HistoryItem),
}

#[derive(Debug, Clone)]
pub struct Compaction {
    pub summary: CompactionSummary,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct StatusChange {
    pub new_status: AgentStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Log {
    pub error: bool,
    pub message: String,
}

pub type InferenceId = u64;

#[derive(Debug, Clone)]
pub struct InferenceResultStart {
    pub id: InferenceId,
    /// Snapshot of the model that produced this result, not a request-start event.
    pub model_config: ModelConfig,
    pub request_config: RequestConfig,
}

#[derive(Debug, Clone)]
pub struct InferenceResultEnd {
    pub id: InferenceId,
    /// Must be terminal. Preserves incomplete/failed/cancelled distinctions.
    pub outcome: Outcome,
    pub usage: Usage,
    pub request_time: Duration,
    pub response_id: Option<String>,
    pub provider_metadata: ProviderMetadata,
}
