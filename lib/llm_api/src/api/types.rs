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
    pub parameters: super::ToolSchema,
    /// Whether to require strict schema adherence. None uses provider defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl ToolDefinition {
    /// Describe deserialized arguments with an explicit strict-mode policy.
    /// `None` uses provider defaults; `Some(false)` preserves non-strict behavior.
    pub fn for_type<T: schemars::JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
        strict: Option<bool>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: super::ToolSchema::for_type::<T>(),
            strict,
        }
    }
}

/// One API response, independent of the model-facing conversation history.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// Model config that produced this response.
    pub config: ModelConfig,
    pub response_id: Option<String>,
    pub outcome: Outcome,
    pub items: Vec<HistoryItem>,
    pub usage: Usage,
    /// Extra fields provider adds
    pub provider_metadata: ProviderFields,
}

impl ModelResponse {
    /// Empty in-progress response, before any data has streamed in.
    pub fn new(config: ModelConfig) -> Self {
        Self {
            config,
            response_id: None,
            outcome: Outcome::InProgress,
            items: Vec::new(),
            usage: Usage::default(),
            provider_metadata: ProviderFields::default(),
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
    ProviderItem(ProviderData),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryItemMetadata {
    pub item_id: Option<String>,
    pub item_status: Option<ItemStatus>,
    /// The provider's native form of this item, replayed in place of a
    /// rebuilt one so provider-only fields survive. Dropped on model change.
    pub provider_data: Option<ProviderData>,
}

/// Provider lifecycle status of an output item, serialized as its wire string.
/// Hosted tool items may report other statuses, such as `searching`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Incomplete,
    Other(String),
}

impl ItemStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Other(status) => status,
        }
    }

    /// Whether the item stopped before finishing; such tool calls cannot be replayed.
    pub fn is_unfinished(&self) -> bool {
        matches!(self, Self::InProgress | Self::Incomplete)
    }
}

impl From<String> for ItemStatus {
    fn from(status: String) -> Self {
        match status.as_str() {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "incomplete" => Self::Incomplete,
            _ => Self::Other(status),
        }
    }
}

impl From<ItemStatus> for String {
    fn from(status: ItemStatus) -> Self {
        match status {
            ItemStatus::Other(status) => status,
            status => status.as_str().to_owned(),
        }
    }
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
    pub opaque_state: Option<ProviderData>,
}

/// A provider's native JSON value, kept verbatim so it can be replayed to the
/// same provider. Where it is attached says what it is: a whole item, a content
/// block, an annotation, or replay state on a portable value. Other providers
/// cannot interpret it, so changing model drops or rejects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderData {
    pub provider: ProviderType,
    pub raw: Value,
}

impl ProviderData {
    /// The value's `type` field, for typed native objects such as hosted tool
    /// items, content parts and citations. Opaque state usually has none.
    pub fn type_name(&self) -> Option<&str> {
        self.raw.get("type").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Audio(AudioBlock),
    File(FileBlock),
    Refusal(RefusalBlock),
    /// A content part without a portable representation.
    ProviderContent(ProviderData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    /// Native citations, preserving offsets and provider-specific fields.
    pub annotations: Vec<ProviderData>,
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
    /// Provider audio reference, such as a generated audio ID.
    pub provider_audio: Option<ProviderData>,
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
    /// Subset of input_tokens written to the provider's prompt cache.
    pub cache_write_input_tokens: Option<u64>,
    /// Subset of output_tokens; do not add to the total.
    pub reasoning_output_tokens: Option<u64>,
    /// Charge reported by the provider, such as OpenRouter credits.
    pub cost: Option<f64>,
    /// The provider's raw usage object, including fields not modeled above.
    pub provider_details: ProviderFields,
}

impl Usage {
    /// True when no count or provider detail was reported at all, such as a
    /// record whose tokens are billed elsewhere. Distinct from reported zeros.
    pub fn is_unreported(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.cache_write_input_tokens.is_none()
            && self.reasoning_output_tokens.is_none()
            && self.cost.is_none()
            && self.provider_details.is_empty()
    }

    /// Replace reported counts (including zero), retaining fields absent from the snapshot.
    pub(super) fn merge_reported(&mut self, latest: Self) {
        self.input_tokens = latest.input_tokens.or(self.input_tokens);
        self.output_tokens = latest.output_tokens.or(self.output_tokens);
        self.total_tokens = latest.total_tokens.or(self.total_tokens);
        self.cached_input_tokens = latest.cached_input_tokens.or(self.cached_input_tokens);
        self.cache_write_input_tokens = latest
            .cache_write_input_tokens
            .or(self.cache_write_input_tokens);
        self.reasoning_output_tokens = latest
            .reasoning_output_tokens
            .or(self.reasoning_output_tokens);
        self.cost = latest.cost.or(self.cost);

        self.provider_details.extend(latest.provider_details);
    }
}

/// Extra provider fields around a portable value, such as response envelope
/// fields. Carried for persistence and debugging; nothing reads its keys.
pub type ProviderFields = BTreeMap<String, Value>;

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
            cache_write_input_tokens: Some(3),
            reasoning_output_tokens: Some(1),
            cost: Some(0.5),
            provider_details: BTreeMap::from([
                ("retained".into(), json!(true)),
                ("updated".into(), json!("old")),
            ]),
        };

        usage.merge_reported(Usage {
            output_tokens: Some(0),
            total_tokens: Some(10),
            reasoning_output_tokens: Some(0),
            cost: Some(0.25),
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
        assert_eq!(usage.cache_write_input_tokens, Some(3));
        assert_eq!(usage.reasoning_output_tokens, Some(0));
        assert_eq!(usage.cost, Some(0.25));
        assert_eq!(usage.provider_details["retained"], json!(true));
        assert_eq!(usage.provider_details["updated"], json!("new"));
        assert_eq!(usage.provider_details["added"], json!(true));
    }
}
