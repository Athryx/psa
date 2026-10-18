//! Common history, model exchange, and content types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;
use crate::providers::{ModelConfig, ProviderType};

/// A function tool the model can request. Execution belongs to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's argument object.
    pub parameters: Value,
    /// Whether to require strict schema adherence. None uses provider defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// A prior request/response pair retained for replay.
/// `generate` requires an output with a terminal outcome for each stored turn.
/// `None` may be used while assembling history but is not valid for replay.
#[derive(Debug, Clone, Default)]
pub struct ModelTurn {
    pub input: ModelTurnInput,
    pub output: Option<ModelTurnOutput>,
}

/// Only the new items for a single request, not a repeated copy of history.
#[derive(Debug, Clone, Default)]
pub struct ModelTurnInput {
    pub items: Vec<HistoryItem>,
}

/// One API response
#[derive(Debug, Clone)]
pub struct ModelTurnOutput {
    /// Model config which returned the request
    pub config: ModelConfig,
    pub response_id: Option<String>,
    pub outcome: Outcome,
    pub items: Vec<HistoryItem>,
    pub usage: Usage,
    /// Extra fields provider adds
    pub provider_metadata: ProviderMetadata,
}

impl ModelTurnOutput {
    /// Initial default state before data starts streaming in
    pub fn default_for_model(config: ModelConfig) -> Self {
        Self {
            config,
            response_id: None,
            outcome: Outcome::InProgress,
            items: Vec::new(),
            usage: Usage::default(),
            provider_metadata: ProviderMetadata::default(),
        }
    }
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
    // used when response is still streaming
    InProgress,
    Completed { reason: StopReason },
    Incomplete { reason: StopReason },
    Failed(Error),
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

impl Usage {
    /// Replace reported counts (including zero), retaining fields absent from the snapshot.
    pub(super) fn merge_reported(&mut self, latest: Self) {
        self.input_tokens = latest.input_tokens.or(self.input_tokens);
        self.output_tokens = latest.output_tokens.or(self.output_tokens);
        self.total_tokens = latest.total_tokens.or(self.total_tokens);
        self.cached_input_tokens = latest.cached_input_tokens.or(self.cached_input_tokens);
        self.reasoning_output_tokens = latest
            .reasoning_output_tokens
            .or(self.reasoning_output_tokens);

        self.provider_details.extend(latest.provider_details);
    }
}

pub type ProviderMetadata = BTreeMap<String, Value>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_snapshots_replace_reported_fields_and_retain_absent_fields() {
        let mut usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            total_tokens: Some(12),
            cached_input_tokens: Some(4),
            reasoning_output_tokens: Some(1),
            provider_details: BTreeMap::from([
                ("retained".into(), json!(true)),
                ("updated".into(), json!("old")),
            ]),
        };

        usage.merge_reported(Usage {
            output_tokens: Some(0),
            total_tokens: Some(10),
            reasoning_output_tokens: Some(0),
            provider_details: BTreeMap::from([
                ("updated".into(), json!("new")),
                ("added".into(), json!(true)),
            ]),
            ..Usage::default()
        });

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(0));
        assert_eq!(usage.total_tokens, Some(10));
        assert_eq!(usage.cached_input_tokens, Some(4));
        assert_eq!(usage.reasoning_output_tokens, Some(0));
        assert_eq!(usage.provider_details["retained"], json!(true));
        assert_eq!(usage.provider_details["updated"], json!("new"));
        assert_eq!(usage.provider_details["added"], json!(true));
    }
}
