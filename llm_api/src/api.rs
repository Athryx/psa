//! Common history, model exchange, and content types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::providers::{ModelConfig, ProviderType};

pub trait ApiProvider {}

/// Canonical, editable history. Ranges use indices into `items`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentHistory {
    pub items: Vec<HistoryItem>,
    pub exchanges: Vec<ModelExchange>,
    /// Editing history invalidates checkpoints that depend on the edited items.
    pub checkpoints: Vec<ContinuationCheckpoint>,
}

/// One API request/response exchange, not an entire agent tool loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelExchange {
    /// History supplied to the model, including any prior conversation.
    pub input: std::ops::Range<usize>,
    /// Items appended by this response.
    pub output: std::ops::Range<usize>,
    pub config: ModelConfig,
    pub response_id: Option<String>,
    pub outcome: Outcome,
    pub usage: Usage,
    pub provider_metadata: ProviderMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuationCheckpoint {
    pub provider: ProviderType,
    /// Exclusive end of the history prefix represented by this checkpoint.
    pub history_end: usize,
    /// Provider response/conversation IDs and other continuation state.
    pub state: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub kind: HistoryItemKind,
    pub metadata: HistoryItemMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HistoryItemKind {
    Message(Message),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Reasoning(Reasoning),
    /// Hosted tools and other items without a portable representation.
    ProviderItem(ProviderItem),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryItemMetadata {
    pub item_id: Option<String>,
    pub item_status: Option<String>,
    pub provider_metadata: ProviderMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// Preserve the original string, including malformed or incomplete JSON.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Vec<ContentBlock>,
    /// Normalized harness flag; not every provider has an equivalent field.
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reasoning {
    pub summary: Vec<String>,
    /// Provider-specific encrypted state, signatures, or other replay data.
    pub opaque_state: Option<ProviderOpaqueData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOpaqueData {
    pub provider: ProviderType,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderItem {
    pub provider: ProviderType,
    pub item_type: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Audio(AudioBlock),
    File(FileBlock),
    Refusal(RefusalBlock),
    ProviderContent(ProviderContent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub provider: ProviderType,
    pub annotation_type: String,
    /// Preserve citation offsets and provider-specific reference fields.
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBlock {
    pub source: MediaSource,
    pub detail: Option<ImageDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageDetail {
    Auto,
    Low,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioBlock {
    /// May be absent when audio is represented by a provider audio ID.
    pub source: Option<MediaSource>,
    pub format: Option<String>,
    pub transcript: Option<String>,
    pub provider_audio: Option<ProviderOpaqueData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBlock {
    pub source: MediaSource,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefusalBlock {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderContent {
    pub provider: ProviderType,
    pub content_type: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaSource {
    Url(String),
    Inline {
        mime_type: String,
        data: Vec<u8>,
    },
    /// File identifiers are scoped to their provider.
    ProviderFile {
        provider: ProviderType,
        file_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Completed { reason: StopReason },
    Incomplete { reason: StopReason },
    Failed(ApiError),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndTurn,
    ToolCalls,
    StopSequence,
    TokenLimit,
    ContentFilter,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: Option<String>,
    pub message: String,
    pub provider_metadata: ProviderMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// None means unreported, rather than zero.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Subset of input_tokens; do not add to the total.
    pub cached_input_tokens: Option<u64>,
    /// Subset of output_tokens; do not add to the total.
    pub reasoning_output_tokens: Option<u64>,
    pub provider_details: ProviderMetadata,
}

pub type ProviderMetadata = BTreeMap<String, Value>;
