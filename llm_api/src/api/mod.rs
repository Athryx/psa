//! Common API interface and provider implementations.
//!
//! # Streaming contract
//!
//! Events describe changes already applied to `ModelTurnStream::current()`;
//! the snapshot is authoritative, not an independently replayable event log.
//! Item, content-block, and reasoning-summary indices are stable and append-only
//! within a response. Updates to different items may be interleaved.
//!
//! Adapters buffer tool-call identity until both the complete name and call ID
//! are known before emitting `ItemAdded`. Any arguments received meanwhile must
//! be preserved. Tool argument deltas may be invalid/partial JSON; callers must
//! wait for `ItemCompleted` before executing a tool.
//!
//! Final content/item snapshots may correct accumulated drafts or add fields
//! absent from deltas. Consumers must refresh snapshots on completion rather
//! than append the final text a second time. Completed items receive no further
//! updates. Usage notifications replace reported counts, never increment them;
//! unreported counts remain `None`.
//!
//! # Request validation
//!
//! Before sending HTTP, adapters must validate the history, new input, tool
//! definitions, and request configuration. Reject duplicate/empty tool names,
//! invalid parameter schemas, missing forced tools, required tools with an
//! empty tool list, zero output-token limits, and model/provider mismatches.
//! Reject malformed tool-call/result relationships (missing, duplicate, or
//! unresolved calls), and history containing nonterminal outputs. Do not require
//! historical tool names to remain in the current tool list.
//!
//! Unsupported settings, content, or provider-specific replay state must return
//! an explicit error, never be silently dropped. Validate what is knowable
//! locally; provider-side schema/capability validation may still reject requests.
//!
//! History contains prior input/output pairs, each input holding only new items
//! for that request. Serialize those pairs in order, then append the separately
//! supplied new input exactly once. Incomplete/cancelled/failed output may be
//! replayed only if structurally valid for the target API; otherwise the caller
//! must repair or remove it. Never execute a partial tool call during replay.

pub mod agent_history;
mod openai_completions;
mod openai_responses;
pub mod types;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::providers::{ModelConfig, ProviderType};

pub use agent_history::{AgentHistory, validate_request};
pub use types::*;

/// Per-request settings. Unset options use provider/model defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestConfig {
    pub model: ModelConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Allows multiple tool calls in one response, not concurrent execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl RequestConfig {
    pub fn new(model: ModelConfig) -> Self {
        Self {
            model,
            max_output_tokens: None,
            tool_choice: None,
            parallel_tool_calls: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    /// Require a call to this named function tool.
    Function {
        name: String,
    },
}

/// Providers support use from multithreaded executors; generation futures are Send.
pub trait ApiProvider: Send + Sync {
    /// Owned, Send stream with provider-specific parsing and accumulation.
    /// It must not borrow the provider, history, input, or request configuration.
    type Stream: ModelTurnStream + 'static;

    /// Provider served by this client instance.
    fn provider_type(&self) -> ProviderType;

    /// Validates and sends a new request without modifying history.
    /// Success means the HTTP stream is established and `current()` is initialized
    /// with the requested model and an in-progress outcome. Provider response IDs
    /// may arrive later and are reported via `TurnEvent::MetadataUpdated`.
    /// Validation failures occur before HTTP; subsequent failures use the stream.
    fn generate(
        &self,
        history: &AgentHistory,
        input: &ModelTurnInput,
        config: &RequestConfig,
    ) -> impl std::future::Future<Output = Result<Self::Stream, Error>> + Send {
        async move {
            validate_request(history, self.provider_type(), input, config)?;
            self.generate_inner(history, input, config).await
        }
    }

    /// Implement adapter-specific validation, serialization, and HTTP setup.
    /// Called by `generate` only after common validation. Implementors should
    /// override this method, not `generate`; direct calls bypass common checks.
    fn generate_inner(
        &self,
        history: &AgentHistory,
        input: &ModelTurnInput,
        config: &RequestConfig,
    ) -> impl std::future::Future<Output = Result<Self::Stream, Error>> + Send;
}

/// Provider-specific streaming response handle.
/// State advances only when `next_event` is polled (or `close` is called).
///
/// Implementations must release the HTTP response/connection and stop any
/// associated local work when dropped, just as when explicitly closed. Neither
/// dropping nor closing guarantees immediate server-side cancellation or stops
/// billing for work already performed. Background jobs require separate
/// provider-specific server cancellation.
/// Handles and their polling futures must be Send; handles need not be Sync.
pub trait ModelTurnStream: Send {
    /// Latest accumulated response, including any partial output.
    fn current(&self) -> &ModelTurnOutput;

    /// Applies each event to the current output before returning it.
    /// A provider terminal outcome emits `Finished` exactly once, then `None`.
    /// Transport/protocol failures (including unexpected EOF) set a failed
    /// outcome, return `Err` once, then `None`, without emitting `Finished`.
    /// Partial output and available usage are preserved on failure.
    /// After termination or explicit closure, all calls return `None`.
    ///
    /// Must be cancellation-safe: dropping this future must not lose buffered
    /// bytes, pending events, or state changes. Parser state belongs to the
    /// handle, so polling again resumes without skipped or duplicated updates.
    fn next_event(
        &mut self,
    ) -> impl std::future::Future<Output = Option<Result<TurnEvent, Error>>> + Send;

    /// Immediately stops local streaming and releases the HTTP response.
    /// Idempotent. Preserves partial items and usage. If still in progress,
    /// sets the outcome to `Outcome::Cancelled`; terminal outcomes are retained.
    /// Subsequent `next_event` calls return `None` without a `Finished` event.
    /// This does not send a server-side cancellation request.
    fn close(&mut self);
}

/// Notifications describe changes already applied to `current()`.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// The item already exists. Tool calls have a complete name and call ID.
    ItemAdded {
        index: usize,
    },
    ItemUpdated {
        index: usize,
        update: ItemUpdate,
    },
    /// The final authoritative item snapshot is available; refresh the item.
    ItemCompleted {
        index: usize,
    },
    /// Usage is an updated snapshot, not a token-count increment.
    UsageUpdated,
    /// Response metadata changed, including a newly available provider response ID.
    MetadataUpdated,
    /// Terminal outcome, including incomplete, failed, or cancelled responses.
    Finished,
}

/// Changes within an existing item, already reflected in the current snapshot.
#[derive(Debug, Clone)]
pub enum ItemUpdate {
    /// The content block already exists in the message.
    ContentAdded {
        content_index: usize,
    },
    ContentDelta {
        content_index: usize,
        delta: ContentDelta,
    },
    /// Refresh a block for changes without a portable delta (e.g. citations).
    ContentUpdated {
        content_index: usize,
    },
    /// Refresh the final block snapshot, which may correct accumulated deltas.
    ContentCompleted {
        content_index: usize,
    },
    ToolArgumentsDelta {
        delta: String,
    },
    ReasoningSummaryAdded {
        summary_index: usize,
    },
    ReasoningSummaryDelta {
        summary_index: usize,
        delta: String,
    },
    /// Changes without a portable delta representation, including opaque state.
    MetadataUpdated,
    ProviderUpdated,
}

/// Append-only string fragments within an existing content block.
/// Non-text media and provider-specific changes use `ContentUpdated` snapshots.
#[derive(Debug, Clone)]
pub enum ContentDelta {
    /// Append to `TextBlock::text`.
    Text(String),
    /// Append to `RefusalBlock::text`.
    Refusal(String),
    /// Append to `AudioBlock::transcript`, treating an absent transcript as empty.
    AudioTranscript(String),
}
