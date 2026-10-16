//! Streaming Chat Completions client for OpenRouter-compatible endpoints.
//!
//! The URL is the complete endpoint URL, not a base URL. This adapter identifies
//! as `Openrouter`, including when pointed at a compatible local server, and uses
//! OpenRouter's `reasoning: { effort }` rather than OpenAI's `reasoning_effort`.
//! Native `reasoning` text and `reasoning_details` are retained for replay.
//! Display-only reasoning summaries without native replay state are unsupported.
//! Tool identity can be fragmented without an end marker, so tool calls first
//! appear at `finish_reason`, with their buffered arguments already populated.

use std::collections::{BTreeMap, HashSet, VecDeque};

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::Error;
use crate::providers::ProviderType;

use super::transport::{
    ApiClient, Decoder, SseEvent, StreamingResponse, protocol, provider_error, usage,
};
use super::{
    AgentHistory, Annotation, ApiProvider, ContentBlock, ContentDelta, HistoryItem,
    HistoryItemKind, HistoryItemMetadata, ImageDetail, ItemUpdate, MediaSource, Message,
    ModelTurnInput, ModelTurnOutput, ModelTurnStream, Outcome, ProviderOpaqueData, Reasoning,
    RefusalBlock, RequestConfig, Role, StopReason, TextBlock, ToolCall, ToolChoice, TurnEvent,
    validate_request,
};

/// Cloneable HTTP client. Clones share the underlying connection pool.
#[derive(Clone)]
pub struct OpenAiCompletions {
    client: ApiClient,
}

impl OpenAiCompletions {
    /// Construct a client using the supplied full endpoint URL verbatim.
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, Error> {
        Ok(Self {
            client: ApiClient::new(api_url, api_key)?,
        })
    }
}

/// Owned, cancellation-safe Chat Completions response stream.
pub struct OpenAiCompletionsStream {
    inner: StreamingResponse<CompletionsDecoder>,
}

impl ApiProvider for OpenAiCompletions {
    type Stream = OpenAiCompletionsStream;

    fn provider_type(&self) -> ProviderType {
        ProviderType::Openrouter
    }

    async fn generate_inner(
        &self,
        history: &AgentHistory,
        input: &ModelTurnInput,
        config: &RequestConfig,
    ) -> Result<Self::Stream, Error> {
        let body = request_body(history, input, config)?;
        let response = self.client.post(&body).await?;
        Ok(OpenAiCompletionsStream {
            inner: StreamingResponse::new(
                response,
                ModelTurnOutput::default_for_model(history.config.clone()),
                CompletionsDecoder::default(),
            ),
        })
    }
}

impl ModelTurnStream for OpenAiCompletionsStream {
    fn current(&self) -> &ModelTurnOutput {
        self.inner.current()
    }

    async fn next_event(&mut self) -> Option<Result<TurnEvent, Error>> {
        self.inner.next_event().await
    }

    fn close(&mut self) {
        self.inner.close();
    }

    fn into_output(self) -> ModelTurnOutput {
        self.inner.into_output()
    }
}

fn unsupported(feature: impl Into<String>) -> Error {
    Error::Unsupported {
        feature: feature.into(),
    }
}

fn request_body(
    history: &AgentHistory,
    input: &ModelTurnInput,
    config: &RequestConfig,
) -> Result<Value, Error> {
    // Also protect direct generate_inner callers; all checks precede HTTP.
    validate_request(history, ProviderType::Openrouter, input, config)?;
    let mut messages = Vec::new();
    for turn in &history.turns {
        append_messages(&turn.input.items, &mut messages)?;
        let output = turn.output.as_ref().expect("validated historical output");
        append_messages(&output.items, &mut messages)?;
    }
    append_messages(&input.items, &mut messages)?;

    let mut body = json!({
        "model": history.config.model.api_name(),
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "reasoning": {"effort": history.config.effort.as_str()},
    });
    if let Some(limit) = config.max_output_tokens {
        body["max_completion_tokens"] = json!(limit);
    }
    if let Some(parallel) = config.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(parallel);
    }
    if let Some(choice) = &config.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function { name } => {
                json!({"type": "function", "function": {"name": name}})
            }
        };
    }
    if !history.tools.is_empty() {
        let mut tools = Vec::with_capacity(history.tools.len());
        for tool in &history.tools {
            super::schema::validate_tool(tool)?;
            let mut function = json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            });
            if let Some(strict) = tool.strict {
                function["strict"] = json!(strict);
            }
            tools.push(json!({"type": "function", "function": function}));
        }
        body["tools"] = json!(tools);
    }
    Ok(body)
}

#[derive(Default)]
struct AssistantMessage {
    content: Option<Value>,
    refusal: Option<String>,
    calls: Vec<Value>,
    reasoning: Option<String>,
    reasoning_details: Vec<Value>,
    has_message: bool,
}

impl AssistantMessage {
    fn flush(&mut self, messages: &mut Vec<Value>) {
        if !self.has_message
            && self.calls.is_empty()
            && self.reasoning.is_none()
            && self.reasoning_details.is_empty()
        {
            return;
        }
        let mut message = json!({
            "role": "assistant",
            "content": self.content.take().unwrap_or(Value::Null),
        });
        if let Some(refusal) = self.refusal.take() {
            message["refusal"] = json!(refusal);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = json!(std::mem::take(&mut self.calls));
        }
        if let Some(reasoning) = self.reasoning.take() {
            message["reasoning"] = json!(reasoning);
        }
        if !self.reasoning_details.is_empty() {
            message["reasoning_details"] = json!(std::mem::take(&mut self.reasoning_details));
        }
        messages.push(message);
        self.has_message = false;
    }
}

/// Each input/output slice is a grouping boundary. In particular, calls from a
/// new output must never attach to an assistant message from a previous turn.
fn append_messages(items: &[HistoryItem], messages: &mut Vec<Value>) -> Result<(), Error> {
    let mut assistant = AssistantMessage::default();
    for item in items {
        if !item.metadata.provider_metadata.is_empty() {
            return Err(unsupported("Chat Completions item replay metadata"));
        }
        match &item.kind {
            HistoryItemKind::Message(message) if message.role == Role::Assistant => {
                if assistant.has_message {
                    assistant.flush(messages);
                }
                let (content, refusal) = message_content(&message.content, Role::Assistant)?;
                assistant.content = Some(content);
                assistant.refusal = refusal;
                assistant.has_message = true;
            }
            HistoryItemKind::ToolCall(call) => {
                // Arguments are a wire string, not necessarily valid JSON. A
                // completed malformed call and its error result are replayable;
                // common validation rejects unfinished calls by item status.
                assistant.calls.push(json!({
                    "id": call.call_id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                }));
            }
            HistoryItemKind::Reasoning(reasoning) => {
                let state = reasoning.opaque_state.as_ref().ok_or_else(|| {
                    unsupported("reasoning replay without native OpenRouter state")
                })?;
                if state.provider != ProviderType::Openrouter {
                    return Err(Error::ProviderMismatch {
                        expected: ProviderType::Openrouter,
                        actual: state.provider,
                    });
                }
                let state = state
                    .data
                    .as_object()
                    .filter(|object| {
                        !object.is_empty()
                            && object.keys().all(|key| {
                                matches!(key.as_str(), "reasoning" | "reasoning_details")
                            })
                    })
                    .ok_or_else(|| unsupported("unrecognized OpenRouter reasoning replay state"))?;
                if let Some(reasoning) = state.get("reasoning") {
                    let reasoning = reasoning.as_str().ok_or_else(|| {
                        unsupported("non-string OpenRouter reasoning replay state")
                    })?;
                    assistant
                        .reasoning
                        .get_or_insert_with(String::new)
                        .push_str(reasoning);
                }
                if let Some(details) = state.get("reasoning_details") {
                    let details = details
                        .as_array()
                        .filter(|details| {
                            !details.is_empty() && details.iter().all(Value::is_object)
                        })
                        .ok_or_else(|| {
                            unsupported("unrecognized OpenRouter reasoning_details replay state")
                        })?;
                    assistant.reasoning_details.extend(details.iter().cloned());
                }
            }
            HistoryItemKind::Message(message) => {
                assistant.flush(messages);
                let (content, _) = message_content(&message.content, message.role)?;
                messages.push(json!({"role": role_name(message.role), "content": content}));
            }
            HistoryItemKind::ToolResult(result) => {
                assistant.flush(messages);
                // Chat Completions has no is_error flag. The caller's supplied
                // error text is sent unchanged, just like any other result.
                let content = text_content(&result.content)?;
                messages.push(
                    json!({"role": "tool", "tool_call_id": result.call_id, "content": content}),
                );
            }
            HistoryItemKind::ProviderItem(_) => {
                return Err(unsupported(
                    "native provider items in Chat Completions history",
                ));
            }
        }
    }
    assistant.flush(messages);
    Ok(())
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn text_content(blocks: &[ContentBlock]) -> Result<String, Error> {
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(block) if block.annotations.is_empty() => text.push_str(&block.text),
            _ => {
                return Err(unsupported(
                    "non-text content or annotations in Chat Completions tool results",
                ));
            }
        }
    }
    Ok(text)
}

fn message_content(blocks: &[ContentBlock], role: Role) -> Result<(Value, Option<String>), Error> {
    let mut parts = Vec::new();
    let mut refusal: Option<String> = None;
    let mut has_images = false;
    for block in blocks {
        match block {
            ContentBlock::Text(block) => {
                if !block.annotations.is_empty() {
                    return Err(unsupported(
                        "replaying text annotations in Chat Completions",
                    ));
                }
                parts.push(json!({"type": "text", "text": block.text}));
            }
            ContentBlock::Refusal(block) if role == Role::Assistant => {
                refusal
                    .get_or_insert_with(String::new)
                    .push_str(&block.text);
            }
            ContentBlock::Image(image) if role == Role::User => {
                let url = match &image.source {
                    MediaSource::Url(url) if !url.is_empty() => url.clone(),
                    MediaSource::Inline { mime_type, data }
                        if mime_type.starts_with("image/") && !data.is_empty() =>
                    {
                        format!(
                            "data:{mime_type};base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(data)
                        )
                    }
                    MediaSource::ProviderFile { .. } => {
                        return Err(unsupported("provider file IDs as Chat Completions images"));
                    }
                    _ => {
                        return Err(Error::InvalidRequest {
                            message:
                                "images require a nonempty URL or inline image MIME type and bytes"
                                    .into(),
                        });
                    }
                };
                let mut image_url = json!({"url": url});
                if let Some(detail) = image.detail {
                    image_url["detail"] = json!(match detail {
                        ImageDetail::Auto => "auto",
                        ImageDetail::Low => "low",
                        ImageDetail::High => "high",
                    });
                }
                parts.push(json!({"type": "image_url", "image_url": image_url}));
                has_images = true;
            }
            ContentBlock::Image(_) => {
                return Err(unsupported(
                    "images outside user messages in Chat Completions",
                ));
            }
            ContentBlock::Refusal(_) => {
                return Err(unsupported(
                    "refusals outside assistant messages in Chat Completions",
                ));
            }
            ContentBlock::Audio(_) => {
                return Err(unsupported("Chat Completions audio input/replay"));
            }
            ContentBlock::File(_) => return Err(unsupported("Chat Completions file input/replay")),
            ContentBlock::ProviderContent(_) => {
                return Err(unsupported("native provider content in Chat Completions"));
            }
        }
    }
    let content = if has_images {
        Value::Array(parts)
    } else if parts.is_empty() && role == Role::Assistant {
        Value::Null
    } else {
        Value::String(
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect(),
        )
    };
    Ok((content, refusal))
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct CompletionsDecoder {
    message: Option<usize>,
    text: Option<usize>,
    refusal: Option<usize>,
    reasoning: Option<usize>,
    reasoning_text: Option<String>,
    reasoning_details: BTreeMap<u64, Value>,
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
}

impl Decoder for CompletionsDecoder {
    fn decode(
        &mut self,
        event: SseEvent,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<bool, Error> {
        let data = event.data.trim();
        if data == "[DONE]" {
            let reason = self
                .finish_reason
                .as_deref()
                .ok_or_else(|| protocol("[DONE] before a finish_reason"))?;
            output.outcome = outcome(reason);
            return Ok(true);
        }
        if data.is_empty() {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(data)?;
        let object = value
            .as_object()
            .ok_or_else(|| protocol("Chat Completions event must be an object"))?;
        if let Some(error) = object.get("error").filter(|error| !error.is_null()) {
            return Err(provider_error(error));
        }
        if event.event.as_deref() == Some("error") {
            return Err(provider_error(&value));
        }
        let mut metadata_changed = false;
        if let Some(id) = object.get("id").filter(|id| !id.is_null()) {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| protocol("response id must be a nonempty string"))?;
            if output
                .response_id
                .as_deref()
                .is_some_and(|previous| previous != id)
            {
                return Err(protocol("response id changed during streaming"));
            }
            if output.response_id.is_none() {
                output.response_id = Some(id.to_owned());
                metadata_changed = true;
            }
        }
        for (key, value) in object {
            if !matches!(key.as_str(), "id" | "choices" | "usage" | "error")
                && output.provider_metadata.get(key) != Some(value)
            {
                output.provider_metadata.insert(key.clone(), value.clone());
                metadata_changed = true;
            }
        }
        if metadata_changed {
            events.push_back(TurnEvent::MetadataUpdated);
        }
        if let Some(value) = object.get("usage").filter(|value| !value.is_null()) {
            if !value.is_object() {
                return Err(protocol("usage must be an object"));
            }
            let incoming = usage(value, true);
            macro_rules! replace_reported {
                ($($field:ident),*) => {$(
                    if incoming.$field.is_some() {
                        output.usage.$field = incoming.$field;
                    }
                )*};
            }
            replace_reported!(
                input_tokens,
                output_tokens,
                total_tokens,
                cached_input_tokens,
                reasoning_output_tokens
            );
            output
                .usage
                .provider_details
                .extend(incoming.provider_details);
            events.push_back(TurnEvent::UsageUpdated);
        }
        let choices = object
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol("Chat Completions event requires a choices array"))?;
        if choices.is_empty() {
            return Ok(false);
        }
        if choices.len() != 1 || choices[0].get("index").and_then(Value::as_u64) != Some(0) {
            return Err(unsupported(
                "Chat Completions responses with choices other than a single index 0",
            ));
        }
        let choice = choices[0]
            .as_object()
            .ok_or_else(|| protocol("choice must be an object"))?;
        let delta = match choice.get("delta") {
            Some(Value::Object(delta)) => delta,
            None | Some(Value::Null)
                if choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null()) =>
            {
                // Some compatible servers omit an empty final delta.
                static EMPTY: std::sync::LazyLock<Map<String, Value>> =
                    std::sync::LazyLock::new(Map::new);
                &EMPTY
            }
            _ => return Err(protocol("streaming choice requires an object delta")),
        };
        if self.finish_reason.is_some() {
            return Err(protocol("choice received after finish_reason"));
        }
        self.decode_delta(delta, output, events)?;
        for (key, value) in choice {
            if !matches!(key.as_str(), "index" | "delta" | "finish_reason") {
                output
                    .provider_metadata
                    .insert(format!("choice_{key}"), value.clone());
                events.push_back(TurnEvent::MetadataUpdated);
            }
        }
        if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
            let reason = reason
                .as_str()
                .filter(|reason| !reason.is_empty())
                .ok_or_else(|| protocol("finish_reason must be a nonempty string"))?;
            self.finish(reason, output, events)?;
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(false)
    }
}

fn outcome(reason: &str) -> Outcome {
    if reason == "error" {
        return Outcome::Failed(Error::Provider {
            code: Some("error".into()),
            message: "Chat Completions provider terminated generation with finish_reason=error"
                .into(),
            metadata: Default::default(),
        });
    }
    let reason = match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolCalls,
        "length" => StopReason::TokenLimit,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    };
    match reason {
        StopReason::TokenLimit | StopReason::ContentFilter => Outcome::Incomplete { reason },
        _ => Outcome::Completed { reason },
    }
}

impl CompletionsDecoder {
    fn decode_delta(
        &mut self,
        delta: &Map<String, Value>,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        for (key, value) in delta {
            if !value.is_null()
                && !matches!(
                    key.as_str(),
                    "role"
                        | "content"
                        | "refusal"
                        | "tool_calls"
                        | "reasoning"
                        | "reasoning_content"
                        | "reasoning_details"
                        | "annotations"
                )
            {
                return Err(unsupported(format!("Chat Completions delta field {key}")));
            }
        }
        if let Some(role) = delta.get("role").filter(|value| !value.is_null())
            && role.as_str() != Some("assistant")
        {
            return Err(protocol("streamed message role is not assistant"));
        }
        for (key, refusal) in [("content", false), ("refusal", true)] {
            if let Some(value) = delta.get(key).filter(|value| !value.is_null()) {
                let text = value
                    .as_str()
                    .ok_or_else(|| unsupported(format!("non-string Chat Completions {key}")))?;
                self.append_text(text, refusal, output, events);
            }
        }
        if let Some(annotations) = delta.get("annotations").filter(|value| !value.is_null()) {
            let annotations = annotations
                .as_array()
                .ok_or_else(|| protocol("annotations must be an array"))?;
            if !annotations.is_empty() {
                self.append_text("", false, output, events);
                let message_index = self.message.unwrap();
                let content_index = self.text.unwrap();
                if let HistoryItemKind::Message(message) = &mut output.items[message_index].kind
                    && let ContentBlock::Text(text) = &mut message.content[content_index]
                {
                    text.annotations.extend(annotations.iter().map(|raw| {
                        Annotation {
                            provider: ProviderType::Openrouter,
                            annotation_type: raw
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_owned(),
                            raw: raw.clone(),
                        }
                    }));
                }
                events.push_back(TurnEvent::ItemUpdated {
                    index: message_index,
                    update: ItemUpdate::ContentUpdated { content_index },
                });
            }
        }
        let reasoning_text = delta.get("reasoning").filter(|value| !value.is_null());
        let reasoning_alias = delta
            .get("reasoning_content")
            .filter(|value| !value.is_null());
        if reasoning_text.is_some()
            && reasoning_alias.is_some()
            && reasoning_text != reasoning_alias
        {
            return Err(unsupported(
                "conflicting reasoning and reasoning_content fields",
            ));
        }
        if let Some(value) = reasoning_text.or(reasoning_alias) {
            let text = value
                .as_str()
                .ok_or_else(|| protocol("reasoning text must be a string"))?;
            self.reasoning_text
                .get_or_insert_with(String::new)
                .push_str(text);
            let index = self.ensure_reasoning(output, events);
            if let HistoryItemKind::Reasoning(reasoning) = &mut output.items[index].kind {
                if reasoning.summary.is_empty() {
                    reasoning.summary.push(String::new());
                    events.push_back(TurnEvent::ItemUpdated {
                        index,
                        update: ItemUpdate::ReasoningSummaryAdded { summary_index: 0 },
                    });
                }
                reasoning.summary[0].push_str(text);
                if !text.is_empty() {
                    events.push_back(TurnEvent::ItemUpdated {
                        index,
                        update: ItemUpdate::ReasoningSummaryDelta {
                            summary_index: 0,
                            delta: text.to_owned(),
                        },
                    });
                }
            }
            self.update_reasoning_state(output, events);
        }
        if let Some(details) = delta
            .get("reasoning_details")
            .filter(|value| !value.is_null())
        {
            self.append_reasoning_details(details, output, events)?;
        }
        if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
            let calls = calls
                .as_array()
                .ok_or_else(|| protocol("tool_calls delta must be an array"))?;
            for call in calls {
                let call = call
                    .as_object()
                    .ok_or_else(|| protocol("tool call delta must be an object"))?;
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| protocol("tool call delta requires a nonnegative index"))?;
                if call
                    .get("type")
                    .is_some_and(|kind| !kind.is_null() && kind.as_str() != Some("function"))
                {
                    return Err(unsupported("non-function Chat Completions tools"));
                }
                for (key, value) in call {
                    if !value.is_null()
                        && !matches!(key.as_str(), "index" | "type" | "id" | "function")
                    {
                        return Err(unsupported(format!("tool call delta field {key}")));
                    }
                }
                let pending = self.calls.entry(index).or_default();
                append_fragment(call.get("id"), &mut pending.id, "tool call id")?;
                if let Some(function) = call.get("function").filter(|value| !value.is_null()) {
                    let function = function
                        .as_object()
                        .ok_or_else(|| protocol("tool call function must be an object"))?;
                    for (key, value) in function {
                        if !value.is_null() && !matches!(key.as_str(), "name" | "arguments") {
                            return Err(unsupported(format!("function call delta field {key}")));
                        }
                    }
                    append_fragment(function.get("name"), &mut pending.name, "tool name")?;
                    append_fragment(
                        function.get("arguments"),
                        &mut pending.arguments,
                        "tool arguments",
                    )?;
                }
            }
        }
        Ok(())
    }

    fn append_text(
        &mut self,
        fragment: &str,
        refusal: bool,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) {
        let index = *self.message.get_or_insert_with(|| {
            let index = output.items.len();
            output.items.push(HistoryItem {
                kind: HistoryItemKind::Message(Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                }),
                metadata: streaming_metadata(),
            });
            events.push_back(TurnEvent::ItemAdded { index });
            index
        });
        let HistoryItemKind::Message(message) = &mut output.items[index].kind else {
            unreachable!()
        };
        let slot = if refusal {
            &mut self.refusal
        } else {
            &mut self.text
        };
        let content_index = *slot.get_or_insert_with(|| {
            let content_index = message.content.len();
            message.content.push(if refusal {
                ContentBlock::Refusal(RefusalBlock {
                    text: String::new(),
                })
            } else {
                ContentBlock::Text(TextBlock {
                    text: String::new(),
                    annotations: Vec::new(),
                })
            });
            events.push_back(TurnEvent::ItemUpdated {
                index,
                update: ItemUpdate::ContentAdded { content_index },
            });
            content_index
        });
        if !fragment.is_empty() {
            let delta = match &mut message.content[content_index] {
                ContentBlock::Text(text) => {
                    text.text.push_str(fragment);
                    ContentDelta::Text(fragment.to_owned())
                }
                ContentBlock::Refusal(text) => {
                    text.text.push_str(fragment);
                    ContentDelta::Refusal(fragment.to_owned())
                }
                _ => unreachable!(),
            };
            events.push_back(TurnEvent::ItemUpdated {
                index,
                update: ItemUpdate::ContentDelta {
                    content_index,
                    delta,
                },
            });
        }
    }

    fn ensure_reasoning(
        &mut self,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> usize {
        *self.reasoning.get_or_insert_with(|| {
            let index = output.items.len();
            output.items.push(HistoryItem {
                kind: HistoryItemKind::Reasoning(Reasoning::default()),
                metadata: streaming_metadata(),
            });
            events.push_back(TurnEvent::ItemAdded { index });
            index
        })
    }

    fn append_reasoning_details(
        &mut self,
        details: &Value,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let details = details
            .as_array()
            .ok_or_else(|| protocol("reasoning_details must be an array"))?;
        if details.is_empty() {
            return Ok(());
        }
        for (position, detail) in details.iter().enumerate() {
            let detail = detail
                .as_object()
                .ok_or_else(|| protocol("reasoning detail must be an object"))?;
            let index = if let Some(index) = detail.get("index").filter(|value| !value.is_null()) {
                index
                    .as_u64()
                    .ok_or_else(|| protocol("reasoning detail index must be nonnegative"))?
            } else if let Some(id) = detail.get("id").filter(|value| !value.is_null()) {
                // An unindexed detail with a new ID is a new block, even when
                // it is the sole entry in a later delta array. Array position
                // is not a stable identity across chunks.
                if let Some(index) = self.reasoning_details.iter().find_map(|(index, previous)| {
                    (previous.get("id") == Some(id)).then_some(*index)
                }) {
                    index
                } else {
                    let mut index = 0_u64;
                    while self.reasoning_details.contains_key(&index) {
                        index = index
                            .checked_add(1)
                            .ok_or_else(|| protocol("too many reasoning details"))?;
                    }
                    index
                }
            } else {
                position as u64
            };
            let entry = self
                .reasoning_details
                .entry(index)
                .or_insert_with(|| json!({}));
            let entry = entry
                .as_object_mut()
                .expect("reasoning details are objects");
            for (key, value) in detail {
                // Optional native fields often arrive as explicit nulls. They
                // carry no fragment or identity, and must not erase established
                // state or prevent an identity from being supplied later.
                if value.is_null()
                    && matches!(
                        key.as_str(),
                        "index"
                            | "id"
                            | "type"
                            | "format"
                            | "text"
                            | "summary"
                            | "data"
                            | "signature"
                    )
                {
                    continue;
                }
                if matches!(key.as_str(), "text" | "summary" | "data" | "signature") {
                    let fragment = value
                        .as_str()
                        .ok_or_else(|| protocol("reasoning detail fragments must be strings"))?;
                    let accumulated = entry.entry(key.clone()).or_insert_with(|| json!(""));
                    let accumulated = accumulated
                        .as_str()
                        .expect("reasoning fragments are strings");
                    let combined = format!("{accumulated}{fragment}");
                    entry.insert(key.clone(), json!(combined));
                } else {
                    if matches!(key.as_str(), "id" | "type" | "format")
                        && entry
                            .get(key)
                            .is_some_and(|previous| !previous.is_null() && previous != value)
                    {
                        return Err(protocol(
                            "reasoning detail identity changed during streaming",
                        ));
                    }
                    entry.insert(key.clone(), value.clone());
                }
            }
        }
        self.ensure_reasoning(output, events);
        self.update_reasoning_state(output, events);
        Ok(())
    }

    fn update_reasoning_state(
        &self,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) {
        let index = self.reasoning.expect("reasoning item was initialized");
        let mut data = Map::new();
        if let Some(text) = &self.reasoning_text {
            data.insert("reasoning".into(), json!(text));
        }
        if !self.reasoning_details.is_empty() {
            data.insert(
                "reasoning_details".into(),
                json!(self.reasoning_details.values().collect::<Vec<_>>()),
            );
        }
        if let HistoryItemKind::Reasoning(reasoning) = &mut output.items[index].kind {
            reasoning.opaque_state = Some(ProviderOpaqueData {
                provider: ProviderType::Openrouter,
                data: Value::Object(data),
            });
        }
        events.push_back(TurnEvent::ItemUpdated {
            index,
            update: ItemUpdate::ProviderUpdated,
        });
    }

    fn finish(
        &mut self,
        reason: &str,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        // Names and IDs are arbitrary fragments. There is no safe early boundary
        // for identity in this protocol, so calls remain private until finish.
        let mut ids = HashSet::new();
        for call in self.calls.values() {
            if call.id.trim().is_empty() || call.name.trim().is_empty() {
                return Err(protocol(
                    "finished tool call is missing its complete name or call ID",
                ));
            }
            if !ids.insert(call.id.as_str()) {
                return Err(protocol("duplicate tool call ID in response"));
            }
        }
        if reason == "tool_calls" && self.calls.is_empty() {
            return Err(protocol("tool_calls finish_reason without tool calls"));
        }
        let status = if matches!(reason, "length" | "content_filter" | "error") {
            "incomplete"
        } else {
            "completed"
        };
        // Complete only previously existing items here; newly appended calls are
        // completed below and never mutated after their completion notification.
        for index in 0..output.items.len() {
            if let HistoryItemKind::Message(message) = &output.items[index].kind {
                for content_index in 0..message.content.len() {
                    events.push_back(TurnEvent::ItemUpdated {
                        index,
                        update: ItemUpdate::ContentCompleted { content_index },
                    });
                }
            }
            output.items[index].metadata.item_status = Some(status.into());
            events.push_back(TurnEvent::ItemCompleted { index });
        }
        for call in std::mem::take(&mut self.calls).into_values() {
            let index = output.items.len();
            output.items.push(HistoryItem {
                kind: HistoryItemKind::ToolCall(ToolCall {
                    call_id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                }),
                metadata: HistoryItemMetadata {
                    item_status: Some(status.into()),
                    ..HistoryItemMetadata::default()
                },
            });
            // ItemAdded contains the buffered arguments already. Re-emitting
            // those fragments as deltas would make consumers append them twice.
            events.push_back(TurnEvent::ItemAdded { index });
            events.push_back(TurnEvent::ItemCompleted { index });
        }
        Ok(())
    }
}

fn streaming_metadata() -> HistoryItemMetadata {
    HistoryItemMetadata {
        item_status: Some("in_progress".into()),
        ..HistoryItemMetadata::default()
    }
}

fn append_fragment(value: Option<&Value>, target: &mut String, field: &str) -> Result<(), Error> {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        target.push_str(
            value
                .as_str()
                .ok_or_else(|| protocol(format!("{field} fragment must be a string")))?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AudioBlock, ImageBlock, ModelTurn, ToolDefinition, ToolResult};
    use crate::providers::{Effort, Model, ModelConfig};

    fn model() -> ModelConfig {
        ModelConfig {
            model: Model {
                model_slug: "openrouter/vendor/model".into(),
            },
            provider: ProviderType::Openrouter,
            effort: Effort::High,
        }
    }

    fn item(kind: HistoryItemKind) -> HistoryItem {
        HistoryItem {
            kind,
            metadata: HistoryItemMetadata::default(),
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text(TextBlock {
            text: text.into(),
            annotations: Vec::new(),
        })
    }

    fn message(role: Role, value: &str) -> HistoryItem {
        item(HistoryItemKind::Message(Message {
            role,
            content: vec![text(value)],
        }))
    }

    fn call(id: &str, name: &str) -> HistoryItem {
        item(HistoryItemKind::ToolCall(ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }))
    }

    fn result(id: &str) -> HistoryItem {
        item(HistoryItemKind::ToolResult(ToolResult {
            call_id: id.into(),
            content: vec![text("result")],
            is_error: true,
        }))
    }

    fn output(items: Vec<HistoryItem>) -> ModelTurnOutput {
        let mut output = ModelTurnOutput::default_for_model(model());
        output.outcome = Outcome::Completed {
            reason: StopReason::EndTurn,
        };
        output.items = items;
        output
    }

    fn native_reasoning() -> HistoryItem {
        item(HistoryItemKind::Reasoning(Reasoning {
            summary: vec!["display summary".into()],
            opaque_state: Some(ProviderOpaqueData {
                provider: ProviderType::Openrouter,
                data: json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"secret","id":"r1","format":"native"}]}),
            }),
        }))
    }

    fn decode(
        decoder: &mut CompletionsDecoder,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
        value: Value,
    ) -> Result<bool, Error> {
        decoder.decode(
            SseEvent {
                event: None,
                data: value.to_string(),
            },
            output,
            events,
        )
    }

    fn chunk(delta: Value, reason: Value) -> Value {
        json!({"id":"response-1","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }

    fn done(
        decoder: &mut CompletionsDecoder,
        output: &mut ModelTurnOutput,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<bool, Error> {
        decoder.decode(
            SseEvent {
                event: None,
                data: "[DONE]".into(),
            },
            output,
            events,
        )
    }

    #[test]
    fn serializes_history_once_and_keeps_assistant_tool_groups() {
        let mut history = AgentHistory::new(model());
        history.turns = vec![
            ModelTurn {
                input: ModelTurnInput {
                    items: vec![message(Role::User, "old")],
                },
                output: Some(output(vec![message(Role::Assistant, "unrelated")])),
            },
            ModelTurn {
                input: ModelTurnInput {
                    items: vec![message(Role::User, "next")],
                },
                output: Some(output(vec![
                    native_reasoning(),
                    message(Role::Assistant, "checking"),
                    call("a", "one"),
                    call("b", "two"),
                ])),
            },
        ];
        history.tools.push(ToolDefinition {
            name: "current_tool".into(),
            description: "current definition, independent of historical names".into(),
            parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            strict: Some(true),
        });
        let input = ModelTurnInput {
            items: vec![result("a"), result("b"), message(Role::User, "new")],
        };
        let config = RequestConfig {
            max_output_tokens: Some(99),
            parallel_tool_calls: Some(false),
            tool_choice: Some(ToolChoice::Function {
                name: "current_tool".into(),
            }),
        };
        let body = request_body(&history, &input, &config).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 7);
        assert_eq!(messages[0]["content"], "old");
        assert_eq!(messages[1]["content"], "unrelated");
        assert!(messages[1].get("tool_calls").is_none());
        assert_eq!(messages[2]["content"], "next");
        assert_eq!(messages[3]["content"], "checking");
        assert_eq!(messages[3]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[3]["tool_calls"][0]["id"], "a");
        assert_eq!(messages[3]["tool_calls"][1]["id"], "b");
        assert_eq!(messages[3]["reasoning_details"][0]["data"], "secret");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "a");
        assert_eq!(messages[5]["tool_call_id"], "b");
        assert_eq!(messages[6]["content"], "new");
        assert_eq!(body["model"], "vendor/model");
        assert_eq!(body["reasoning"], json!({"effort":"high"}));
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert_eq!(body["max_completion_tokens"], 99);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            body["tool_choice"],
            json!({"type":"function","function":{"name":"current_tool"}})
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn does_not_attach_new_calls_to_prior_assistant_message() {
        let mut messages = Vec::new();
        append_messages(&[message(Role::Assistant, "previous")], &mut messages).unwrap();
        append_messages(&[call("a", "first"), call("b", "second")], &mut messages).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].get("tool_calls").is_none());
        assert_eq!(messages[1]["content"], Value::Null);
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
        let mut messages = Vec::new();
        append_messages(
            &[
                message(Role::Assistant, "first"),
                call("a", "first"),
                result("a"),
                message(Role::Assistant, "second"),
                call("b", "second"),
            ],
            &mut messages,
        )
        .unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["tool_calls"][0]["id"], "a");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "b");
    }

    #[test]
    fn preserves_malformed_but_completed_tool_arguments_for_error_recovery() {
        let mut malformed = call("a", "test");
        if let HistoryItemKind::ToolCall(call) = &mut malformed.kind {
            call.arguments = "{invalid".into();
        }
        let mut messages = Vec::new();
        append_messages(&[malformed, result("a")], &mut messages).unwrap();
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            "{invalid"
        );
    }

    #[test]
    fn encodes_inline_and_url_images_with_detail() {
        let history = AgentHistory::new(model());
        let input = ModelTurnInput {
            items: vec![item(HistoryItemKind::Message(Message {
                role: Role::User,
                content: vec![
                    text("look"),
                    ContentBlock::Image(ImageBlock {
                        source: MediaSource::Inline {
                            mime_type: "image/png".into(),
                            data: vec![0, 1, 2],
                        },
                        detail: Some(ImageDetail::High),
                    }),
                    ContentBlock::Image(ImageBlock {
                        source: MediaSource::Url("https://example.com/image.png".into()),
                        detail: None,
                    }),
                ],
            }))],
        };
        let body = request_body(&history, &input, &RequestConfig::default()).unwrap();
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0], json!({"type":"text","text":"look"}));
        assert_eq!(
            content[1]["image_url"],
            json!({"url":"data:image/png;base64,AAEC","detail":"high"})
        );
        assert_eq!(
            content[2]["image_url"]["url"],
            "https://example.com/image.png"
        );
    }

    #[test]
    fn unsupported_content_and_reasoning_fail_during_serialization() {
        let history = AgentHistory::new(model());
        let input = ModelTurnInput {
            items: vec![item(HistoryItemKind::Message(Message {
                role: Role::User,
                content: vec![ContentBlock::Audio(AudioBlock {
                    source: None,
                    format: None,
                    transcript: Some("not a text substitute".into()),
                    provider_audio: None,
                })],
            }))],
        };
        assert!(matches!(
            request_body(&history, &input, &RequestConfig::default()),
            Err(Error::Unsupported { .. })
        ));
        let input = ModelTurnInput {
            items: vec![item(HistoryItemKind::Reasoning(Reasoning {
                summary: vec!["not native".into()],
                opaque_state: None,
            }))],
        };
        assert!(matches!(
            request_body(&history, &input, &RequestConfig::default()),
            Err(Error::Unsupported { .. })
        ));
        let mut input = ModelTurnInput {
            items: vec![message(Role::User, "hi")],
        };
        input.items[0]
            .metadata
            .provider_metadata
            .insert("native".into(), json!("state"));
        assert!(matches!(
            request_body(&history, &input, &RequestConfig::default()),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn validates_schema_shapes_and_tool_choices_before_http() {
        let mut history = AgentHistory::new(model());
        history.tools.push(ToolDefinition {
            name: "test".into(),
            description: String::new(),
            parameters: json!({"type":42}),
            strict: None,
        });
        assert!(matches!(
            request_body(
                &history,
                &ModelTurnInput::default(),
                &RequestConfig::default()
            ),
            Err(Error::InvalidToolDefinition { .. })
        ));
        history.tools[0].parameters =
            json!({"type":"object","properties":{"x":{"type":"string"}},"required":["x"]});
        for (choice, expected) in [
            (ToolChoice::Auto, "auto"),
            (ToolChoice::None, "none"),
            (ToolChoice::Required, "required"),
        ] {
            let config = RequestConfig {
                tool_choice: Some(choice),
                ..RequestConfig::default()
            };
            let body = request_body(&history, &ModelTurnInput::default(), &config).unwrap();
            assert_eq!(body["tool_choice"], expected);
            assert!(body["tools"][0]["function"].get("strict").is_none());
        }
    }

    #[test]
    fn decodes_text_refusal_and_replace_only_reported_usage() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        assert!(
            !decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(json!({"role":"assistant","content":"hel"}), Value::Null)
            )
            .unwrap()
        );
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({"content":"lo","refusal":"no"}), Value::Null),
        )
        .unwrap();
        decode(&mut decoder, &mut output, &mut events, json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}})).unwrap();
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({"refusal":" thanks"}), json!("stop")),
        )
        .unwrap();
        assert!(matches!(output.outcome, Outcome::InProgress));
        let frozen = serde_json::to_value(&output.items).unwrap();
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            json!({"choices":[],"usage":{"completion_tokens":5,"total_tokens":15}}),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&output.items).unwrap(), frozen);
        assert_eq!(output.response_id.as_deref(), Some("response-1"));
        assert_eq!(output.usage.input_tokens, Some(10));
        assert_eq!(output.usage.output_tokens, Some(5));
        assert_eq!(output.usage.total_tokens, Some(15));
        assert_eq!(output.usage.cached_input_tokens, Some(4));
        assert_eq!(output.usage.reasoning_output_tokens, Some(2));
        let HistoryItemKind::Message(message) = &output.items[0].kind else {
            panic!()
        };
        assert!(matches!(&message.content[0], ContentBlock::Text(text) if text.text == "hello"));
        assert!(
            matches!(&message.content[1], ContentBlock::Refusal(text) if text.text == "no thanks")
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TurnEvent::ItemAdded { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TurnEvent::ItemCompleted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TurnEvent::UsageUpdated))
                .count(),
            2
        );
        assert!(done(&mut decoder, &mut output, &mut events).unwrap());
        assert!(matches!(
            output.outcome,
            Outcome::Completed {
                reason: StopReason::EndTurn
            }
        ));
        // Only transport emits Finished.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TurnEvent::Finished))
        );
    }

    #[test]
    fn buffers_fragmented_interleaved_tool_identity_and_arguments() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        decode(&mut decoder, &mut output, &mut events, chunk(json!({"tool_calls":[
            {"index":1,"id":"b","type":"function","function":{"name":"sec","arguments":"{"}},
            {"index":0,"id":"ca","function":{"name":"fi","arguments":"{\"x\":"}}
        ]}), Value::Null)).unwrap();
        assert!(output.items.is_empty());
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(
                json!({"tool_calls":[
                    {"index":0,"id":"ll","function":{"name":"rst","arguments":"1}"}},
                    {"index":1,"id":"2","function":{"name":"ond","arguments":"}"}}
                ]}),
                Value::Null,
            ),
        )
        .unwrap();
        assert!(output.items.is_empty());
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({}), json!("tool_calls")),
        )
        .unwrap();
        assert_eq!(output.items.len(), 2);
        let HistoryItemKind::ToolCall(first) = &output.items[0].kind else {
            panic!()
        };
        assert_eq!(
            (&*first.call_id, &*first.name, &*first.arguments),
            ("call", "first", "{\"x\":1}")
        );
        let HistoryItemKind::ToolCall(second) = &output.items[1].kind else {
            panic!()
        };
        assert_eq!(
            (&*second.call_id, &*second.name, &*second.arguments),
            ("b2", "second", "{}")
        );
        let notifications = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    TurnEvent::ItemAdded { .. } | TurnEvent::ItemCompleted { .. }
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            notifications.as_slice(),
            [
                TurnEvent::ItemAdded { index: 0 },
                TurnEvent::ItemCompleted { index: 0 },
                TurnEvent::ItemAdded { index: 1 },
                TurnEvent::ItemCompleted { index: 1 }
            ]
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            TurnEvent::ItemUpdated {
                update: ItemUpdate::ToolArgumentsDelta { .. },
                ..
            }
        )));
        assert!(done(&mut decoder, &mut output, &mut events).unwrap());
        assert!(matches!(
            output.outcome,
            Outcome::Completed {
                reason: StopReason::ToolCalls
            }
        ));
    }

    #[test]
    fn incomplete_calls_cannot_be_replayed() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        decode(&mut decoder, &mut output, &mut events, chunk(json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"test","arguments":"{"}}]}), json!("length"))).unwrap();
        done(&mut decoder, &mut output, &mut events).unwrap();
        assert!(matches!(
            output.outcome,
            Outcome::Incomplete {
                reason: StopReason::TokenLimit
            }
        ));
        assert_eq!(
            output.items[0].metadata.item_status.as_deref(),
            Some("incomplete")
        );
        let mut history = AgentHistory::new(model());
        history.turns.push(ModelTurn {
            input: ModelTurnInput::default(),
            output: Some(output),
        });
        let input = ModelTurnInput {
            items: vec![result("a")],
        };
        assert!(matches!(
            request_body(&history, &input, &RequestConfig::default()),
            Err(Error::InvalidHistory { .. })
        ));
    }

    #[test]
    fn replays_plain_native_reasoning_in_a_tool_loop() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(
                json!({"reasoning":"think", "reasoning_content":"think"}),
                Value::Null,
            ),
        )
        .unwrap();
        decode(&mut decoder, &mut output, &mut events, chunk(json!({"reasoning_content":"ing", "tool_calls":[{"index":0,"id":"a","function":{"name":"test","arguments":"{}"}}]}), json!("tool_calls"))).unwrap();
        done(&mut decoder, &mut output, &mut events).unwrap();
        let HistoryItemKind::Reasoning(reasoning) = &output.items[0].kind else {
            panic!()
        };
        assert_eq!(reasoning.summary, vec!["thinking"]);
        assert_eq!(
            reasoning.opaque_state.as_ref().unwrap().data,
            json!({"reasoning":"thinking"})
        );
        let mut history = AgentHistory::new(model());
        history.turns.push(ModelTurn {
            input: ModelTurnInput {
                items: vec![message(Role::User, "question")],
            },
            output: Some(output),
        });
        let body = request_body(
            &history,
            &ModelTurnInput {
                items: vec![result("a")],
            },
            &RequestConfig::default(),
        )
        .unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(body["messages"][1]["reasoning"], "thinking");
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "a");
        assert_eq!(body["messages"][2]["tool_call_id"], "a");
    }

    #[test]
    fn null_reasoning_fields_are_absent_and_identity_can_arrive_later() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        for detail in [
            json!({"index":0,"id":null,"type":"reasoning.text","format":null,"text":"a","signature":null,"data":null,"summary":null}),
            json!({"index":null,"id":null,"type":null,"format":null,"text":null,"signature":null}),
            json!({"index":0,"id":"r","format":"native","signature":"sig","text":"b"}),
            json!({"index":0,"id":null,"format":null,"signature":"nature","text":null}),
        ] {
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(json!({"reasoning_details":[detail]}), Value::Null),
            )
            .unwrap();
        }
        let HistoryItemKind::Reasoning(reasoning) = &output.items[0].kind else {
            panic!()
        };
        let detail = &reasoning.opaque_state.as_ref().unwrap().data["reasoning_details"][0];
        assert_eq!(detail["id"], "r");
        assert_eq!(detail["format"], "native");
        assert_eq!(detail["text"], "ab");
        assert_eq!(detail["signature"], "signature");
        assert!(detail.get("data").is_none());
        assert!(detail.get("summary").is_none());
        assert!(matches!(
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(
                    json!({"reasoning_details":[{"index":0,"id":"changed"}]}),
                    Value::Null
                )
            ),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn unindexed_reasoning_details_use_ids_not_delta_array_positions() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        for detail in [
            json!({"id":"a","type":"reasoning.text","text":"x"}),
            json!({"id":"b","type":"reasoning.encrypted","data":"y"}),
            json!({"id":"a","text":"z"}),
        ] {
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(json!({"reasoning_details":[detail]}), Value::Null),
            )
            .unwrap();
        }
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({}), json!("stop")),
        )
        .unwrap();
        done(&mut decoder, &mut output, &mut events).unwrap();
        let HistoryItemKind::Reasoning(reasoning) = &output.items[0].kind else {
            panic!()
        };
        let details = &reasoning.opaque_state.as_ref().unwrap().data["reasoning_details"];
        assert_eq!(details.as_array().unwrap().len(), 2);
        assert_eq!(
            details[0],
            json!({"id":"a","type":"reasoning.text","text":"xz"})
        );
        assert_eq!(
            details[1],
            json!({"id":"b","type":"reasoning.encrypted","data":"y"})
        );
    }

    #[test]
    fn retains_and_replays_fragmented_native_reasoning() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        decode(&mut decoder, &mut output, &mut events, chunk(json!({"reasoning":"think", "reasoning_details":[{"index":0,"type":"reasoning.encrypted","data":"ab","id":"r","format":"native"}]}), Value::Null)).unwrap();
        decode(&mut decoder, &mut output, &mut events, chunk(json!({"reasoning":"ing", "reasoning_details":[{"index":0,"type":"reasoning.encrypted","data":"cd"}], "content":"answer"}), json!("stop"))).unwrap();
        done(&mut decoder, &mut output, &mut events).unwrap();
        let HistoryItemKind::Reasoning(reasoning) = &output.items[0].kind else {
            panic!()
        };
        assert_eq!(reasoning.summary, vec!["thinking"]);
        assert_eq!(
            reasoning.opaque_state.as_ref().unwrap().data["reasoning_details"][0]["data"],
            "abcd"
        );
        let mut messages = Vec::new();
        append_messages(&output.items, &mut messages).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["reasoning_details"][0]["data"], "abcd");
        assert_eq!(messages[0]["reasoning"], "thinking");
        assert_eq!(messages[0]["content"], "answer");
    }

    #[test]
    fn rejects_extra_choices_invalid_identity_and_post_completion_updates() {
        for payload in [
            json!({"choices":[{"index":1,"delta":{"content":"x"}}]}),
            json!({"choices":[{"index":0,"delta":{}},{"index":1,"delta":{}}]}),
        ] {
            assert!(
                decode(
                    &mut CompletionsDecoder::default(),
                    &mut ModelTurnOutput::default_for_model(model()),
                    &mut VecDeque::new(),
                    payload
                )
                .is_err()
            );
        }
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        assert!(done(&mut decoder, &mut output, &mut events).is_err());
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({"content":"final"}), json!("stop")),
        )
        .unwrap();
        let frozen = serde_json::to_value(&output.items).unwrap();
        assert!(
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(json!({"content":"late"}), Value::Null)
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&output.items).unwrap(), frozen);
        let mut decoder = CompletionsDecoder::default();
        assert!(
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(
                    json!({"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}),
                    json!("tool_calls")
                )
            )
            .is_err()
        );
    }

    #[test]
    fn error_finish_reason_is_a_failed_provider_outcome() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        decode(
            &mut decoder,
            &mut output,
            &mut events,
            chunk(json!({"content":"partial"}), json!("error")),
        )
        .unwrap();
        assert!(done(&mut decoder, &mut output, &mut events).unwrap());
        assert!(
            matches!(output.outcome, Outcome::Failed(Error::Provider { code: Some(ref code), .. }) if code == "error")
        );
        assert_eq!(
            output.items[0].metadata.item_status.as_deref(),
            Some("incomplete")
        );
    }

    #[test]
    fn provider_errors_and_unsupported_output_are_explicit() {
        let mut decoder = CompletionsDecoder::default();
        let mut output = ModelTurnOutput::default_for_model(model());
        let mut events = VecDeque::new();
        assert!(
            matches!(decode(&mut decoder, &mut output, &mut events, json!({"error":{"message":"bad","code":"denied"}})), Err(Error::Provider { code: Some(code), message, .. }) if code == "denied" && message == "bad")
        );
        assert!(matches!(
            decode(
                &mut decoder,
                &mut output,
                &mut events,
                chunk(json!({"audio":{"id":"audio-1"}}), Value::Null)
            ),
            Err(Error::Unsupported { .. })
        ));
    }
}
