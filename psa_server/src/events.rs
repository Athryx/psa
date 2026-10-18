//! Agent transcript is append only log of immutable events. with events potentially grouped together.

use std::{path::Path, time::Duration};

use llm_api::providers::ModelConfig;
use time::OffsetDateTime;

use crate::{agent_loop::AgentStatus, tools::ToolId};

/// Events are ordered by increasing counter
pub type EventId = u64;

// this schema differs a bit from what is stored in database
#[derive(Debug, Clone)]
pub struct Event {
    id: EventId,
    timestamp: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub enum EventType {
    StatusChange(StatusChange),
    Model(ModelConfig),
    NewContext,
    Message(Message),
    Thinking(Thinking),
    ToolDefinition(ToolId),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Compaction(Compaction),
    Log(Log),
    InferenceStart(InferenceStart),
    InferenceEnd(InferenceEnd),
}

/// Describes source of message.
#[derive(Debug, Clone, Copy)]
pub enum MessageType {
    /// Message sent by human.
    Human,
    /// Message output by agent.
    Agent,
    /// Message injected by harness tools.
    Harness,
    /// Message which should go in system prompt.
    System,
}

/// Describes how a message is delivered.
#[derive(Debug, Clone, Copy)]
pub enum MessageDelivery {
    Queue,
    Steer,
}

/// Text sent to or returned from agent.
#[derive(Debug, Clone)]
pub struct Message {
    pub message_type: MessageType,
    pub delivery: Option<MessageDelivery>,
    // for now, only string, even though we could have image and other stuff, maybe that stuff can only be return from tool
    pub contents: String,
}

/// Thinking tokens emitted by model.
#[derive(Debug, Clone)]
pub struct Thinking {
    pub contents: ProviderItem,
}

pub type ToolCallId = String;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    // Stored unparsed
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    // TODO: non string results
    pub tool_result: String,
    // api often accepts error flags
    pub error: bool,
}

#[derive(Debug, Clone)]
pub struct Compaction {
    pub summary: ProviderItem,
    pub usage: TokenCounts,
}

#[derive(Debug, Clone)]
pub struct StatusChange {
    pub new_status: AgentStatus,
    pub reason: Option<String>,
}

/// Diagnostic log shown in UI, not sent to model.
#[derive(Debug, Clone)]
pub struct Log {
    pub error: bool,
    pub message: String,
}

pub type InferenceId = u64;

#[derive(Debug, Clone, Copy)]
pub struct InferenceStart {
    pub id: InferenceId,
}

#[derive(Debug, Clone)]
pub enum InferenceResult {
    Completed,
    Error(String),
    Interrupted,
    Timeout,
}

#[derive(Debug, Clone)]
pub struct InferenceEnd {
    pub id: InferenceId,
    pub usage: TokenCounts,
    pub request_time: Duration,
    pub result: InferenceResult,
}

/////////////////////////////////////////////////
///// Common Types Among Events             /////
/////////////////////////////////////////////////

/// Item which may or may not be encrypted by provider.
#[derive(Debug, Clone)]
pub struct ProviderItem {
    pub data: String,
    pub encrypted: bool,
}

pub type TokenCount = u64;

#[derive(Debug, Clone, Copy)]
pub struct TokenCounts {
    pub estimate: bool,
    pub input: TokenCount,
    // output includes reasoning, extra reasoning field is optional breakdown if provider provides it
    pub output: TokenCount,
    pub cache_read: TokenCount,
    pub cache_write: TokenCount,
    pub reasoning: Option<TokenCount>,
}
