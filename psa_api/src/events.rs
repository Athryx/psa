//! Immutable transcript events. Sequence numbers are local to an agent lineage.

use crate::{AgentId, AgentState, ToolId};
use llm_api::api::{
    ContentBlock, HistoryItem, HistoryItemMetadata, Outcome, ProviderMetadata, Reasoning,
    RequestConfig, TextBlock, ToolDefinition as ApiToolDefinition, Usage,
};
use llm_api::providers::ModelConfig;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct EventId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// Physical owner, which may be an ancestor of the requested agent.
    pub source_agent_id: AgentId,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub kind: EventType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    Human,
    Agent,
    Harness,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub message_type: MessageType,
    pub content: Vec<ContentBlock>,
    pub metadata: HistoryItemMetadata,
}

impl Message {
    pub fn text(message_type: MessageType, contents: impl Into<String>) -> Self {
        Self {
            message_type,
            content: vec![ContentBlock::Text(TextBlock {
                text: contents.into(),
                annotations: Vec::new(),
            })],
            metadata: HistoryItemMetadata::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thinking {
    pub contents: Reasoning,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub definition: ApiToolDefinition,
}

pub type ToolCallId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// Preserve unparsed arguments, including malformed JSON.
    pub arguments: String,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub content: crate::ToolResult,
    pub error: bool,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompactionSummary {
    Text(String),
    ProviderItem(HistoryItem),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: CompactionSummary,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusChange {
    pub new_status: AgentState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Log {
    pub error: bool,
    pub message: String,
}

pub type InferenceId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResultStart {
    pub id: InferenceId,
    /// Snapshot of the model that produced this result, not a request-start event.
    pub model_config: ModelConfig,
    pub request_config: RequestConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResultEnd {
    pub id: InferenceId,
    /// Must be terminal. Preserves incomplete/failed/cancelled distinctions.
    pub outcome: Outcome,
    pub usage: Usage,
    pub request_time: Duration,
    pub response_id: Option<String>,
    pub provider_metadata: ProviderMetadata,
}
