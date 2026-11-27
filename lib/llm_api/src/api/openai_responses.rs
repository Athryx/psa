//! Streaming client for the OpenAI Responses protocol.
//!
//! The constructor takes the complete endpoint URL, not an API base URL. Native
//! items and the original item fields are retained for stateless replay.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use base64::Engine;
use serde_json::{Value, json};

use crate::Error;
use crate::providers::ProviderType;

use super::agent_history::require_provider;
use super::transport::{
    ApiClient, Decoder, SseEvent, StreamingResponse, parse_usage, protocol, provider_error,
};
use super::*;

/// An authenticated, reusable Responses client. Clones share the HTTP client.
#[derive(Clone)]
pub struct OpenAiResponses {
    client: ApiClient,
    provider: ProviderType,
}

impl OpenAiResponses {
    /// Select the provider identity independently of the Responses wire format.
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<Auth>,
        provider: ProviderType,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: ApiClient::new(api_url.into(), api_key)?,
            provider,
        })
    }
}

impl ApiProvider for OpenAiResponses {
    type Stream = OpenAiResponsesStream;

    fn provider_type(&self) -> ProviderType {
        self.provider
    }

    async fn generate_unvalidated(
        &self,
        history: &AgentHistory,
        config: &RequestConfig,
    ) -> Result<Self::Stream, Error> {
        require_provider(self.provider, history.config.model.provider)?;

        let body = build_request_body(history, config)?;
        let response = self.client.post(&body).await?;

        Ok(OpenAiResponsesStream {
            inner: StreamingResponse::new(
                response,
                ModelResponse::new(history.config.clone()),
                ResponsesDecoder::default(),
            ),
        })
    }
}

/// Owned, cancellation-safe Responses stream.
pub struct OpenAiResponsesStream {
    inner: StreamingResponse<ResponsesDecoder>,
}

impl ModelTurnStream for OpenAiResponsesStream {
    fn current(&self) -> &ModelResponse {
        self.inner.current()
    }

    async fn next_event(&mut self) -> Option<Result<TurnEvent, Error>> {
        self.inner.next_event().await
    }

    fn close(&mut self) {
        self.inner.close();
    }

    fn into_output(self) -> ModelResponse {
        self.inner.into_output()
    }
}

fn unsupported(feature: impl Into<String>) -> Error {
    Error::Unsupported {
        feature: feature.into(),
    }
}

fn build_request_body(history: &AgentHistory, config: &RequestConfig) -> Result<Value, Error> {
    for tool in &history.tools {
        super::schema::validate_tool(tool)?;
    }

    let items = build_request_items(history)?;

    let mut body = json!({
        "model": history.config.model.api_name,
        "input": items,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });

    let reasoning = config
        .reasoning
        .clone()
        .unwrap_or(ReasoningOverride::Effort(history.config.effort));
    if let Some(effort) = reasoning.wire_effort() {
        body["reasoning"] = json!({"effort": effort});
    }

    if let Some(limit) = config.max_output_tokens {
        body["max_output_tokens"] = json!(limit);
    }
    if let Some(parallel) = config.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(parallel);
    }
    if let Some(choice) = &config.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function { name } => json!({"type":"function", "name":name}),
        };
    }

    if !history.tools.is_empty() {
        body["tools"] = Value::Array(
            history
                .tools
                .iter()
                .map(serialize_tool)
                .collect::<Result<_, _>>()?,
        );
    }

    Ok(body)
}

fn build_request_items(history: &AgentHistory) -> Result<Vec<Value>, Error> {
    history
        .items
        .iter()
        .map(|item| serialize_item(history.config.model.provider, item))
        .collect()
}

fn serialize_tool(tool: &ToolDefinition) -> Result<Value, Error> {
    super::schema::validate_tool(tool)?;
    let mut value = json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters.as_value(),
    });

    if let Some(strict) = tool.strict {
        value["strict"] = json!(strict);
    }

    Ok(value)
}

fn parse_replay_item_metadata(provider: ProviderType, item: &HistoryItem) -> Result<Value, Error> {
    let Some(native) = &item.metadata.provider_data else {
        return Ok(json!({}));
    };
    require_provider(provider, native.provider)?;
    if !native.raw.is_object() {
        return Err(unsupported("malformed Responses item replay metadata"));
    }
    Ok(native.raw.clone())
}

fn parse_native_value(native: &ProviderData) -> Result<Value, Error> {
    if !native.raw.is_object() || native.type_name().is_none() {
        return Err(unsupported("native Responses data must be a typed object"));
    }
    Ok(native.raw.clone())
}

fn serialize_item(provider: ProviderType, item: &HistoryItem) -> Result<Value, Error> {
    let mut value = parse_replay_item_metadata(provider, item)?;

    match &item.kind {
        HistoryItemKind::Message(message) => {
            serialize_message(provider, &item.metadata, message, &mut value)?;
        }
        HistoryItemKind::ToolCall(call) => {
            if item
                .metadata
                .item_status
                .as_ref()
                .is_some_and(ItemStatus::is_unfinished)
            {
                return Err(Error::InvalidHistory {
                    message: "cannot replay an unfinished function call".into(),
                });
            }

            // arguments is a wire string, not a parsed JSON value. A completed
            // call containing malformed JSON can be replayed with a tool error
            // so the model can recover; it must not be repaired here.
            value["type"] = json!("function_call");
            value["call_id"] = json!(call.call_id);
            value["name"] = json!(call.name);
            value["arguments"] = json!(call.arguments);
        }
        HistoryItemKind::ToolResult(result) => {
            value["type"] = json!("function_call_output");
            value["call_id"] = json!(result.call_id);
            // Responses supports typed input content in function outputs. In
            // particular, do not flatten attachments into a lossy text string.
            value["output"] = Value::Array(
                result
                    .content
                    .iter()
                    .map(|block| serialize_content(provider, block, false, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            // is_error is a harness flag, not a separate Responses wire field.
        }
        HistoryItemKind::Reasoning(reasoning) => {
            serialize_reasoning(provider, &item.metadata, reasoning, &mut value)?;
        }
        HistoryItemKind::ProviderItem(item) => {
            require_provider(provider, item.provider)?;
            value = parse_native_value(item)?;
        }
    }

    let object = value.as_object_mut().unwrap();
    if let Some(id) = &item.metadata.item_id {
        object.insert("id".into(), json!(id));
    }
    if let Some(status) = &item.metadata.item_status {
        object.insert("status".into(), json!(status.as_str()));
    }

    Ok(value)
}

fn serialize_content(
    provider: ProviderType,
    block: &ContentBlock,
    assistant: bool,
    prior: Option<&Value>,
) -> Result<Value, Error> {
    match block {
        ContentBlock::Text(text) => {
            for annotation in &text.annotations {
                require_provider(provider, annotation.provider)?;
            }

            let kind = if assistant {
                "output_text"
            } else {
                "input_text"
            };
            let mut value = prior
                .filter(|v| v.get("type").and_then(Value::as_str) == Some(kind))
                .cloned()
                .unwrap_or_else(|| json!({}));
            value["type"] = json!(kind);
            value["text"] = json!(text.text);

            if assistant {
                value["annotations"] =
                    Value::Array(text.annotations.iter().map(|a| a.raw.clone()).collect());
            } else {
                // Input citations are audit metadata; the input schema has no
                // annotations field. The text itself is always sent intact.
                value.as_object_mut().unwrap().remove("annotations");
            }
            Ok(value)
        }
        ContentBlock::Refusal(refusal) if assistant => {
            let mut value = prior
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("refusal"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            value["type"] = json!("refusal");
            value["refusal"] = json!(refusal.text);
            Ok(value)
        }
        ContentBlock::Image(image) if !assistant => serialize_image(provider, image),
        ContentBlock::File(file) if !assistant => serialize_file(provider, file),
        ContentBlock::ProviderContent(content) => {
            require_provider(provider, content.provider)?;
            parse_native_value(content)
        }
        ContentBlock::Audio(_) => Err(unsupported("audio input/replay in the Responses adapter")),
        ContentBlock::Refusal(_) => Err(unsupported("refusal content outside assistant messages")),
        ContentBlock::Image(_) | ContentBlock::File(_) => Err(unsupported(
            "image/file content in assistant Responses messages",
        )),
    }
}

fn serialize_message(
    provider: ProviderType,
    metadata: &HistoryItemMetadata,
    message: &Message,
    value: &mut Value,
) -> Result<(), Error> {
    // Portable/converted assistant messages use EasyInputMessage,
    // whose content is input_text. OutputMessage (output_text/refusal)
    // is only valid with a real provider id and status envelope.
    let native_output = message.role == Role::Assistant
        && metadata
            .item_id
            .as_deref()
            .or_else(|| value.get("id").and_then(Value::as_str))
            .is_some_and(|id| !id.is_empty())
        && (metadata.item_status.is_some()
            || value.get("status").and_then(Value::as_str).is_some());
    if message.role == Role::Assistant
        && !native_output
        && (metadata.item_id.is_some()
            || metadata.item_status.is_some()
            || value.get("id").is_some()
            || value.get("status").is_some())
    {
        return Err(unsupported(
            "assistant output-message replay requires both native id and status",
        ));
    }

    let prior = value.get("content").and_then(Value::as_array);
    let content = message
        .content
        .iter()
        .enumerate()
        .map(|(index, block)| {
            if message.role == Role::Assistant
                && !native_output
                && let ContentBlock::Refusal(refusal) = block
            {
                // EasyInputMessage has no refusal part: preserve its
                // exact visible text as input_text on portable replay.
                // The normalized history still retains RefusalBlock.
                return Ok(json!({"type":"input_text","text":refusal.text}));
            }

            serialize_content(
                provider,
                block,
                native_output,
                prior.and_then(|v| v.get(index)),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    value["type"] = json!("message");
    value["role"] = json!(match message.role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
    });
    value["content"] = Value::Array(content);

    Ok(())
}

fn serialize_reasoning(
    provider: ProviderType,
    metadata: &HistoryItemMetadata,
    reasoning: &Reasoning,
    value: &mut Value,
) -> Result<(), Error> {
    if metadata
        .item_id
        .as_deref()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .is_none_or(str::is_empty)
    {
        return Err(unsupported(
            "reasoning replay requires a native Responses item id",
        ));
    }

    let state = reasoning
        .opaque_state
        .as_ref()
        .ok_or_else(|| unsupported("stateless reasoning replay requires encrypted_content"))?;

    value["type"] = json!("reasoning");
    let prior = value.get("summary").and_then(Value::as_array);
    let summary = reasoning
        .summary
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut part = prior
                .and_then(|v| v.get(index))
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            part["type"] = json!("summary_text");
            part["text"] = json!(text);
            part
        })
        .collect();
    value["summary"] = Value::Array(summary);

    value.as_object_mut().unwrap().remove("encrypted_content");
    require_provider(provider, state.provider)?;
    let encrypted = if state.raw.is_string() {
        &state.raw
    } else {
        state
            .raw
            .get("encrypted_content")
            .filter(|v| v.is_string())
            .ok_or_else(|| unsupported("reasoning state without Responses encrypted_content"))?
    };
    value["encrypted_content"] = encrypted.clone();

    Ok(())
}

fn serialize_image(provider: ProviderType, image: &ImageBlock) -> Result<Value, Error> {
    let mut value = json!({"type":"input_image"});
    match &image.source {
        MediaSource::Url(url) => value["image_url"] = json!(url),
        MediaSource::Inline { mime_type, data } => {
            if !mime_type.starts_with("image/") {
                return Err(unsupported(
                    "Responses image data requires an image MIME type",
                ));
            }
            value["image_url"] = json!(encode_data_url(mime_type, data));
        }
        MediaSource::ProviderFile {
            provider: actual,
            file_id,
        } => {
            require_provider(provider, *actual)?;
            value["file_id"] = json!(file_id);
        }
    }

    if let Some(detail) = image.detail {
        value["detail"] = json!(match detail {
            ImageDetail::Auto => "auto",
            ImageDetail::Low => "low",
            ImageDetail::High => "high",
        });
    }

    Ok(value)
}

fn serialize_file(provider: ProviderType, file: &FileBlock) -> Result<Value, Error> {
    let mut value = json!({"type":"input_file"});
    match &file.source {
        MediaSource::Url(url) => value["file_url"] = json!(url),
        MediaSource::Inline { mime_type, data } => {
            if file
                .mime_type
                .as_ref()
                .is_some_and(|hint| hint != mime_type)
            {
                return Err(unsupported("conflicting inline file MIME types"));
            }
            value["file_data"] = json!(encode_data_url(mime_type, data));
        }
        MediaSource::ProviderFile {
            provider: actual,
            file_id,
        } => {
            require_provider(provider, *actual)?;
            value["file_id"] = json!(file_id);
        }
    }

    if let Some(filename) = &file.filename {
        value["filename"] = json!(filename);
    }

    Ok(value)
}

fn encode_data_url(mime_type: &str, data: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

#[derive(Default)]
struct ResponsesDecoder {
    // Provider indices and public indices coincide. Later slots are buffered
    // until all previous slots (including tool identity) can be published.
    slots: BTreeMap<usize, Slot>,
}

struct Slot {
    raw: Value,
    completed: bool,
    completion_emitted: bool,
    content_done: BTreeSet<usize>,
}

impl Slot {
    fn new(raw: Value) -> Self {
        Self {
            raw,
            completed: false,
            completion_emitted: false,
            content_done: BTreeSet::new(),
        }
    }
}

impl ResponsesDecoder {
    fn require_pending_slot(&mut self, index: usize) -> Result<&mut Slot, Error> {
        let slot = self
            .slots
            .get_mut(&index)
            .ok_or_else(|| protocol("event refers to an unknown output item"))?;
        if slot.completed {
            return Err(protocol("update received after output item completion"));
        }

        Ok(slot)
    }

    fn update_output_items(
        &mut self,
        index: usize,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
        update: Option<ItemUpdate>,
    ) -> Result<(), Error> {
        if index < output.items.len() {
            let slot = &self.slots[&index];
            let item = parse_item(output.config.model.provider, &slot.raw, slot.completed)?
                .ok_or_else(|| protocol("published function identity disappeared"))?;
            validate_item_compatibility(&output.items[index], &item)?;
            output.items[index] = item;
            if let Some(update) = update {
                events.push_back(TurnEvent::ItemUpdated { index, update });
            }
        }

        // Never expose index N before N-1, or a call before its identity is known.
        while let Some(slot) = self.slots.get_mut(&output.items.len()) {
            let Some(item) = parse_item(output.config.model.provider, &slot.raw, slot.completed)?
            else {
                break;
            };
            let index = output.items.len();
            output.items.push(item);
            events.push_back(TurnEvent::ItemAdded { index });
            if slot.completed {
                slot.completion_emitted = true;
                events.push_back(TurnEvent::ItemCompleted { index });
            }
        }

        if index < output.items.len() {
            let slot = self.slots.get_mut(&index).unwrap();
            if slot.completed && !slot.completion_emitted {
                slot.completion_emitted = true;
                events.push_back(TurnEvent::ItemCompleted { index });
            }
        }
        Ok(())
    }

    fn apply_item_snapshot(
        &mut self,
        index: usize,
        raw: Value,
        done: bool,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        if !raw.is_object() {
            return Err(protocol("output item is not an object"));
        }

        if let Some(slot) = self.slots.get_mut(&index) {
            if slot.completed {
                if done && matches_completed_item(output.config.model.provider, &slot.raw, &raw)? {
                    return Ok(());
                }
                return Err(protocol("snapshot changed a completed output item"));
            }

            let mut raw = raw;

            // Some compatible servers reveal a function's identity late.
            // An added identity snapshot must not erase buffered arguments.
            if !done
                && raw.get("type").and_then(Value::as_str) == Some("function_call")
                && raw
                    .get("arguments")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                && let Some(arguments) = slot.raw.get("arguments")
            {
                raw["arguments"] = arguments.clone();
            }

            slot.raw = raw;
            slot.completed = done;
        } else {
            let mut slot = Slot::new(raw);
            slot.completed = done;
            self.slots.insert(index, slot);
        }

        self.update_output_items(
            index,
            output,
            events,
            if done {
                None
            } else {
                Some(ItemUpdate::ProviderUpdated)
            },
        )
    }

    fn apply_response_metadata(
        response: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let object = response
            .as_object()
            .ok_or_else(|| protocol("response snapshot is not an object"))?;
        let mut changed = false;

        if let Some(id) = response.get("id").and_then(Value::as_str) {
            if output.response_id.as_deref().is_some_and(|old| old != id) {
                return Err(protocol("response ID changed during streaming"));
            }
            if output.response_id.as_deref() != Some(id) {
                output.response_id = Some(id.into());
                changed = true;
            }
        }

        for (key, value) in object {
            if output.provider_metadata.get(key) != Some(value) {
                output.provider_metadata.insert(key.clone(), value.clone());
                changed = true;
            }
        }
        if changed {
            events.push_back(TurnEvent::MetadataUpdated);
        }

        if let Some(value) = response.get("usage").filter(|v| !v.is_null()) {
            output.usage.merge_reported(parse_usage(value, false));
            events.push_back(TurnEvent::UsageUpdated);
        }
        Ok(())
    }

    fn complete_remaining_items(
        &mut self,
        status: &str,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        // Tolerate servers omitting redundant terminal output snapshots,
        // but still require all indices and complete function identities.
        let indices: Vec<_> = self.slots.keys().copied().collect();
        for index in indices {
            let slot = self.slots.get_mut(&index).unwrap();
            if slot.completed {
                continue;
            }

            if slot.raw.get("status").and_then(Value::as_str) == Some("in_progress") {
                slot.raw["status"] = json!(if status == "completed" {
                    "completed"
                } else {
                    "incomplete"
                });
            }

            slot.completed = true;
            self.update_output_items(index, output, events, None)?;
        }

        Ok(())
    }

    fn apply_terminal_response(
        &mut self,
        kind: &str,
        response: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        Self::apply_response_metadata(response, output, events)?;
        let status = response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| kind.strip_prefix("response.").unwrap_or(kind));
        if !matches!(status, "completed" | "incomplete" | "failed" | "cancelled") {
            return Err(protocol("terminal response has no terminal status"));
        }

        if let Some(items) = response.get("output") {
            let items = items
                .as_array()
                .ok_or_else(|| protocol("response.output is not an array"))?;
            if self
                .slots
                .last_key_value()
                .is_some_and(|(index, _)| *index >= items.len())
            {
                return Err(protocol("terminal response removed an output item"));
            }
            for (index, item) in items.iter().enumerate() {
                self.apply_item_snapshot(index, item.clone(), true, output, events)?;
            }
        } else {
            self.complete_remaining_items(status, output, events)?;
        }

        if output.items.len() != self.slots.len() {
            return Err(protocol(
                "terminal response has missing output indices or function identity",
            ));
        }

        output.outcome = parse_terminal_outcome(status, response, output);

        Ok(())
    }

    fn handle_content_part(
        &mut self,
        kind: &str,
        value: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let index = parse_index(value, "output_index")?;
        let ci = parse_index_field(value, "content_index")?;
        let slot = self.require_pending_slot(index)?;
        apply_item_id(&mut slot.raw, value)?;
        require_type(&slot.raw, "message")?;

        let part = require_field(value, "part")?.clone();
        let content = ensure_array_field(&mut slot.raw, "content")?;
        let added = ci == content.len();
        update_part(content, ci, part)?;

        let update = if kind.ends_with(".done") {
            if slot.content_done.insert(ci) {
                ItemUpdate::ContentCompleted { content_index: ci }
            } else {
                ItemUpdate::ContentUpdated { content_index: ci }
            }
        } else if added {
            ItemUpdate::ContentAdded { content_index: ci }
        } else {
            ItemUpdate::ContentUpdated { content_index: ci }
        };

        self.update_output_items(index, output, events, Some(update))?;

        Ok(())
    }

    fn handle_text_content(
        &mut self,
        kind: &str,
        value: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let index = parse_index(value, "output_index")?;
        let ci = parse_index_field(value, "content_index")?;
        let slot = self.require_pending_slot(index)?;
        apply_item_id(&mut slot.raw, value)?;
        require_type(&slot.raw, "message")?;

        let refusal = kind.starts_with("response.refusal.");
        let done = kind.ends_with(".done");
        if !done && slot.content_done.contains(&ci) {
            return Err(protocol("delta received after content completion"));
        }

        let content = ensure_array_field(&mut slot.raw, "content")?;
        let added = ci == content.len();
        if added {
            content.push(if refusal {
                json!({"type":"refusal","refusal":""})
            } else {
                json!({"type":"output_text","text":"","annotations":[]})
            });
        }
        let part = content
            .get_mut(ci)
            .ok_or_else(|| protocol("non-contiguous content index"))?;
        require_type(part, if refusal { "refusal" } else { "output_text" })?;

        let field = if refusal { "refusal" } else { "text" };
        let fragment = parse_string_field(value, if done { field } else { "delta" })?;
        if done {
            part[field] = json!(fragment);
        } else {
            append_string_field(part, field, fragment)?;
        }

        let update = if done {
            slot.content_done.insert(ci);
            ItemUpdate::ContentCompleted { content_index: ci }
        } else {
            ItemUpdate::ContentDelta {
                content_index: ci,
                delta: if refusal {
                    ContentDelta::Refusal(fragment.into())
                } else {
                    ContentDelta::Text(fragment.into())
                },
            }
        };

        if added && index < output.items.len() {
            // Both updates refer to a block already reflected in current().
            self.update_output_items(
                index,
                output,
                events,
                Some(ItemUpdate::ContentAdded { content_index: ci }),
            )?;
        }

        self.update_output_items(index, output, events, Some(update))?;

        Ok(())
    }

    fn handle_annotation(
        &mut self,
        value: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let index = parse_index(value, "output_index")?;
        let ci = parse_index_field(value, "content_index")?;
        let slot = self.require_pending_slot(index)?;
        apply_item_id(&mut slot.raw, value)?;

        let content = ensure_array_field(&mut slot.raw, "content")?;
        let part = content
            .get_mut(ci)
            .ok_or_else(|| protocol("unknown annotation content index"))?;
        require_type(part, "output_text")?;
        let annotations = ensure_array_field(part, "annotations")?;
        update_part(
            annotations,
            parse_index_field(value, "annotation_index")?,
            require_field(value, "annotation")?.clone(),
        )?;

        self.update_output_items(
            index,
            output,
            events,
            Some(ItemUpdate::ContentUpdated { content_index: ci }),
        )?;

        Ok(())
    }

    fn handle_function_arguments(
        &mut self,
        kind: &str,
        value: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let index = parse_index(value, "output_index")?;
        let slot = self.require_pending_slot(index)?;
        require_type(&slot.raw, "function_call")?;
        apply_item_id(&mut slot.raw, value)?;

        for field in ["name", "call_id"] {
            if let Some(identity) = value.get(field).and_then(Value::as_str) {
                if slot
                    .raw
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|old| !old.is_empty() && old != identity)
                {
                    return Err(protocol("function identity changed"));
                }
                slot.raw[field] = json!(identity);
            }
        }

        let update = if kind.ends_with(".done") {
            slot.raw["arguments"] = json!(parse_string_field(value, "arguments")?);
            ItemUpdate::ProviderUpdated
        } else {
            let delta = parse_string_field(value, "delta")?;
            append_string_field(&mut slot.raw, "arguments", delta)?;
            ItemUpdate::ToolArgumentsDelta {
                delta: delta.into(),
            }
        };

        self.update_output_items(index, output, events, Some(update))?;

        Ok(())
    }

    fn handle_reasoning_summary(
        &mut self,
        kind: &str,
        value: &Value,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<(), Error> {
        let index = parse_index(value, "output_index")?;
        let si = parse_index_field(value, "summary_index")?;
        let slot = self.require_pending_slot(index)?;
        require_type(&slot.raw, "reasoning")?;
        apply_item_id(&mut slot.raw, value)?;

        let summary = ensure_array_field(&mut slot.raw, "summary")?;
        let added = si == summary.len();
        let update = if kind.contains("_part.") {
            update_part(summary, si, require_field(value, "part")?.clone())?;
            if added {
                ItemUpdate::ReasoningSummaryAdded { summary_index: si }
            } else {
                ItemUpdate::MetadataUpdated
            }
        } else {
            if added {
                summary.push(json!({"type":"summary_text","text":""}));
            }
            let part = summary
                .get_mut(si)
                .ok_or_else(|| protocol("non-contiguous reasoning summary index"))?;
            if kind.ends_with(".done") {
                part["text"] = json!(parse_string_field(value, "text")?);
                ItemUpdate::MetadataUpdated
            } else {
                let delta = parse_string_field(value, "delta")?;
                append_string_field(part, "text", delta)?;
                ItemUpdate::ReasoningSummaryDelta {
                    summary_index: si,
                    delta: delta.into(),
                }
            }
        };

        if added && kind.contains("_text.") && index < output.items.len() {
            self.update_output_items(
                index,
                output,
                events,
                Some(ItemUpdate::ReasoningSummaryAdded { summary_index: si }),
            )?;
        }

        self.update_output_items(index, output, events, Some(update))?;

        Ok(())
    }
}

impl Decoder for ResponsesDecoder {
    fn decode(
        &mut self,
        event: SseEvent,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<bool, Error> {
        if event.data.trim().is_empty()
            || matches!(event.event.as_deref(), Some("ping" | "keepalive"))
        {
            return Ok(false);
        }
        if event.data.trim() == "[DONE]" {
            return Err(protocol(
                "Responses stream ended without a terminal response",
            ));
        }

        let value: Value = serde_json::from_str(&event.data)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .ok_or_else(|| protocol("Responses event has no type"))?;

        match kind {
            "error" | "response.error" => {
                return Err(provider_error(value.get("error").unwrap_or(&value)));
            }
            "response.created" | "response.in_progress" | "response.queued" => {
                Self::apply_response_metadata(require_field(&value, "response")?, output, events)?;
            }
            "response.completed"
            | "response.incomplete"
            | "response.failed"
            | "response.cancelled"
            | "response.done" => {
                self.apply_terminal_response(
                    kind,
                    require_field(&value, "response")?,
                    output,
                    events,
                )?;
                return Ok(true);
            }
            "response.output_item.added" | "response.output_item.done" => {
                self.apply_item_snapshot(
                    parse_index(&value, "output_index")?,
                    require_field(&value, "item")?.clone(),
                    kind.ends_with(".done"),
                    output,
                    events,
                )?;
            }
            "response.content_part.added" | "response.content_part.done" => {
                self.handle_content_part(kind, &value, output, events)?;
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => {
                self.handle_text_content(kind, &value, output, events)?;
            }
            "response.output_text.annotation.added" => {
                self.handle_annotation(&value, output, events)?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.handle_function_arguments(kind, &value, output, events)?;
            }
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {
                self.handle_reasoning_summary(kind, &value, output, events)?;
            }
            // Hosted tools and future event types are represented by their
            // native output-item snapshots. Lifecycle/ping events add no text.
            _ => {}
        }
        Ok(false)
    }
}

fn parse_terminal_outcome(status: &str, response: &Value, output: &ModelResponse) -> Outcome {
    match status {
        "completed" => Outcome::Completed {
            reason: if output
                .items
                .iter()
                .any(|item| matches!(item.kind, HistoryItemKind::ToolCall(_)))
            {
                StopReason::ToolCalls
            } else {
                StopReason::EndTurn
            },
        },
        "incomplete" => Outcome::Incomplete {
            reason: match response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                Some("max_output_tokens") => StopReason::TokenLimit,
                Some("content_filter") => StopReason::ContentFilter,
                Some(reason) => StopReason::Other(reason.into()),
                None => StopReason::Other("incomplete".into()),
            },
        },
        "cancelled" => Outcome::Cancelled,
        "failed" => Outcome::Failed(provider_error(
            response
                .get("error")
                .filter(|v| !v.is_null())
                .unwrap_or(response),
        )),
        _ => unreachable!(),
    }
}

fn require_field<'a>(value: &'a Value, field: &str) -> Result<&'a Value, Error> {
    value
        .get(field)
        .ok_or_else(|| protocol(format!("Responses event is missing {field}")))
}

fn parse_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    require_field(value, field)?
        .as_str()
        .ok_or_else(|| protocol(format!("{field} is not a string")))
}

fn parse_index(value: &Value, field: &str) -> Result<usize, Error> {
    parse_index_field(value, field)
}

fn parse_index_field(value: &Value, field: &str) -> Result<usize, Error> {
    require_field(value, field)?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| protocol(format!("invalid {field}")))
}

fn require_type(value: &Value, expected: &str) -> Result<(), Error> {
    if value.get("type").and_then(Value::as_str) != Some(expected) {
        return Err(protocol(format!("expected {expected} item/content")));
    }

    Ok(())
}

fn ensure_array_field<'a>(value: &'a mut Value, field: &str) -> Result<&'a mut Vec<Value>, Error> {
    if value.get(field).is_none() {
        value[field] = json!([]);
    }

    value
        .get_mut(field)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| protocol(format!("{field} is not an array")))
}

fn append_string_field(value: &mut Value, field: &str, delta: &str) -> Result<(), Error> {
    let mut text = value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if value.get(field).is_some_and(|v| !v.is_string()) {
        return Err(protocol(format!("{field} is not a string")));
    }

    text.push_str(delta);
    value[field] = json!(text);
    Ok(())
}

fn update_part(parts: &mut Vec<Value>, index: usize, part: Value) -> Result<(), Error> {
    if !part.is_object() {
        return Err(protocol("content/summary part is not an object"));
    }
    if index == parts.len() {
        parts.push(part);
    } else if let Some(previous) = parts.get_mut(index) {
        if previous.get("type") != part.get("type") {
            return Err(protocol("content/summary type changed"));
        }
        *previous = part;
    } else {
        return Err(protocol("non-contiguous content/summary index"));
    }

    Ok(())
}

fn apply_item_id(raw: &mut Value, event: &Value) -> Result<(), Error> {
    if let Some(id) = event.get("item_id").and_then(Value::as_str) {
        if raw
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|old| old != id)
        {
            return Err(protocol("output item ID changed"));
        }
        raw["id"] = json!(id);
    }

    Ok(())
}

fn parse_item(
    provider: ProviderType,
    raw: &Value,
    complete: bool,
) -> Result<Option<HistoryItem>, Error> {
    let kind = match parse_string_field(raw, "type")? {
        "message" => {
            let role = match raw
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant")
            {
                "system" => Role::System,
                "developer" => Role::Developer,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return Err(protocol("unknown message role")),
            };
            let content = raw
                .get("content")
                .map(|v| {
                    v.as_array()
                        .ok_or_else(|| protocol("message content is not an array"))
                })
                .transpose()?
                .into_iter()
                .flatten()
                .map(|part| parse_content(provider, part))
                .collect::<Result<Vec<_>, _>>()?;
            HistoryItemKind::Message(Message { role, content })
        }
        "function_call" => {
            let name = raw.get("name").and_then(Value::as_str).unwrap_or("");
            let call_id = raw.get("call_id").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || call_id.is_empty() {
                if complete {
                    return Err(protocol("completed function call has no name or call ID"));
                }
                return Ok(None);
            }
            HistoryItemKind::ToolCall(ToolCall {
                name: name.into(),
                call_id: call_id.into(),
                arguments: raw
                    .get("arguments")
                    .map(|_| parse_string_field(raw, "arguments"))
                    .transpose()?
                    .unwrap_or("")
                    .into(),
            })
        }
        "function_call_output" => {
            let output = require_field(raw, "output")?;
            let content = if let Some(text) = output.as_str() {
                vec![ContentBlock::Text(TextBlock {
                    text: text.into(),
                    annotations: Vec::new(),
                })]
            } else {
                output
                    .as_array()
                    .ok_or_else(|| protocol("function output is neither text nor typed content"))?
                    .iter()
                    .map(|part| parse_content(provider, part))
                    .collect::<Result<Vec<_>, _>>()?
            };
            HistoryItemKind::ToolResult(ToolResult {
                call_id: parse_string_field(raw, "call_id")?.into(),
                content,
                is_error: false,
            })
        }
        "reasoning" => {
            let summary = raw
                .get("summary")
                .map(|v| {
                    v.as_array()
                        .ok_or_else(|| protocol("reasoning summary is not an array"))
                })
                .transpose()?
                .into_iter()
                .flatten()
                .map(|part| parse_string_field(part, "text").map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?;
            HistoryItemKind::Reasoning(Reasoning {
                summary,
                opaque_state: raw
                    .get("encrypted_content")
                    .filter(|v| !v.is_null())
                    .map(|data| ProviderData {
                        provider,
                        raw: data.clone(),
                    }),
            })
        }
        _ => HistoryItemKind::ProviderItem(ProviderData {
            provider,
            raw: raw.clone(),
        }),
    };
    Ok(Some(HistoryItem {
        kind,
        metadata: HistoryItemMetadata {
            item_id: raw.get("id").and_then(Value::as_str).map(str::to_owned),
            item_status: raw
                .get("status")
                .and_then(Value::as_str)
                .map(|status| ItemStatus::from(status.to_owned())),
            provider_data: Some(ProviderData {
                provider,
                raw: raw.clone(),
            }),
        },
    }))
}

fn parse_content(provider: ProviderType, raw: &Value) -> Result<ContentBlock, Error> {
    Ok(match parse_string_field(raw, "type")? {
        "output_text" | "input_text" => ContentBlock::Text(TextBlock {
            text: parse_string_field(raw, "text")?.into(),
            annotations: raw
                .get("annotations")
                .map(|v| {
                    v.as_array()
                        .ok_or_else(|| protocol("annotations is not an array"))
                })
                .transpose()?
                .into_iter()
                .flatten()
                .map(|raw| ProviderData {
                    provider,
                    raw: raw.clone(),
                })
                .collect(),
        }),
        "refusal" => ContentBlock::Refusal(RefusalBlock {
            text: parse_string_field(raw, "refusal")?.into(),
        }),
        _ => ContentBlock::ProviderContent(ProviderData {
            provider,
            raw: raw.clone(),
        }),
    })
}

// A terminal envelope may add provider-only fields to an already final typed
// item. Retain that envelope in response metadata, but never mutate an item
// after ItemCompleted. Changes to portable content/identity remain errors.
fn matches_completed_item(
    provider: ProviderType,
    previous: &Value,
    next: &Value,
) -> Result<bool, Error> {
    let mut previous = parse_item(provider, previous, true)?.unwrap();
    let mut next = parse_item(provider, next, true)?.unwrap();
    previous.metadata.provider_data = None;
    next.metadata.provider_data = None;
    previous.metadata.item_status = None;
    next.metadata.item_status = None;
    Ok(serde_json::to_value(previous)? == serde_json::to_value(next)?)
}

fn validate_item_compatibility(previous: &HistoryItem, next: &HistoryItem) -> Result<(), Error> {
    if previous
        .metadata
        .item_id
        .as_ref()
        .zip(next.metadata.item_id.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(protocol("output item ID changed"));
    }
    match (&previous.kind, &next.kind) {
        (HistoryItemKind::Message(a), HistoryItemKind::Message(b)) => {
            if a.role != b.role || a.content.len() > b.content.len() {
                return Err(protocol(
                    "message role changed or content blocks were removed",
                ));
            }
            for (a, b) in a.content.iter().zip(&b.content) {
                if std::mem::discriminant(a) != std::mem::discriminant(b) {
                    return Err(protocol("content block type changed"));
                }
            }
        }
        (HistoryItemKind::ToolCall(a), HistoryItemKind::ToolCall(b))
            if a.name == b.name && a.call_id == b.call_id => {}
        (HistoryItemKind::Reasoning(a), HistoryItemKind::Reasoning(b))
            if a.summary.len() <= b.summary.len() => {}
        (HistoryItemKind::ProviderItem(a), HistoryItemKind::ProviderItem(b))
            if a.type_name() == b.type_name() => {}
        (HistoryItemKind::ToolResult(a), HistoryItemKind::ToolResult(b))
            if a.call_id == b.call_id => {}
        _ => return Err(protocol("output item type or identity changed")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Effort, Model, ModelConfig};

    const PROVIDER: ProviderType = ProviderType::OpenAiCodex;

    // Keep the original wire-format fixtures concise; generic-identity tests
    // below exercise the explicit provider-taking helpers directly.
    fn serialize_item(item: &HistoryItem) -> Result<Value, Error> {
        super::serialize_item(PROVIDER, item)
    }

    fn serialize_content(
        block: &ContentBlock,
        assistant: bool,
        prior: Option<&Value>,
    ) -> Result<Value, Error> {
        super::serialize_content(PROVIDER, block, assistant, prior)
    }

    fn parse_item(raw: &Value, complete: bool) -> Result<Option<HistoryItem>, Error> {
        super::parse_item(PROVIDER, raw, complete)
    }

    fn parse_content(raw: &Value) -> Result<ContentBlock, Error> {
        super::parse_content(PROVIDER, raw)
    }

    fn model() -> ModelConfig {
        ModelConfig {
            model: Model::new(PROVIDER, "gpt-test").unwrap(),
            effort: Effort::High,
        }
    }

    fn item(kind: HistoryItemKind) -> HistoryItem {
        HistoryItem {
            kind,
            metadata: HistoryItemMetadata::default(),
        }
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text(TextBlock {
            text: value.into(),
            annotations: Vec::new(),
        })
    }

    fn message(role: Role, value: &str) -> HistoryItem {
        item(HistoryItemKind::Message(Message {
            role,
            content: vec![text(value)],
        }))
    }

    struct Harness {
        decoder: ResponsesDecoder,
        output: ModelResponse,
        events: VecDeque<TurnEvent>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                decoder: ResponsesDecoder::default(),
                output: ModelResponse::new(model()),
                events: VecDeque::new(),
            }
        }

        fn feed(&mut self, value: Value) -> Result<bool, Error> {
            self.decoder.decode(
                SseEvent {
                    event: value.get("type").and_then(Value::as_str).map(str::to_owned),
                    data: value.to_string(),
                },
                &mut self.output,
                &mut self.events,
            )
        }

        fn added(&mut self, index: usize, item: Value) {
            self.feed(
                json!({"type":"response.output_item.added","output_index":index,"item":item}),
            )
            .unwrap();
        }

        fn done(&mut self, index: usize, item: Value) {
            self.feed(json!({"type":"response.output_item.done","output_index":index,"item":item}))
                .unwrap();
        }

        fn finish(&mut self, output: Value) {
            assert!(self.feed(json!({"type":"response.completed","response":{"id":"resp","status":"completed","output":output}})).unwrap());
            assert!(
                !self
                    .events
                    .iter()
                    .any(|event| matches!(event, TurnEvent::Finished))
            );
        }
    }

    fn draft_message() -> Value {
        json!({"id":"msg","type":"message","role":"assistant","status":"in_progress","content":[]})
    }

    #[test]
    fn request_serializes_flat_history_exactly_once() {
        let mut history = AgentHistory::new(model());
        let mut output = ModelResponse::new(model());
        output.outcome = Outcome::Completed {
            reason: StopReason::EndTurn,
        };
        output.items.push(message(Role::Assistant, "old reply"));
        history.items.push(message(Role::User, "old input"));
        history.items.extend(output.items);
        history.tools.push(ToolDefinition {
            name: "read".into(),
            description: "read file".into(),
            parameters: json!({"type":"object","properties":{},"additionalProperties":false})
                .into(),
            strict: Some(true),
        });
        history.items.push(message(Role::User, "new input"));
        let config = RequestConfig {
            reasoning: None,
            max_output_tokens: Some(123),
            parallel_tool_calls: Some(false),
            tool_choice: Some(ToolChoice::Function {
                name: "read".into(),
            }),
        };
        let body = build_request_body(&history, &config).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][0]["content"][0]["text"], "old input");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
        assert!(body["input"][1].get("id").is_none());
        assert!(body["input"][1].get("status").is_none());
        assert_eq!(body["input"][2]["content"][0]["text"], "new input");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(
            body["tool_choice"],
            json!({"type":"function","name":"read"})
        );
        assert_eq!(body["max_output_tokens"], 123);
        assert_eq!(body["parallel_tool_calls"], false);
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn all_tool_choices_and_optional_settings_are_serialized() {
        let history = AgentHistory::new(model());
        for (choice, expected) in [
            (ToolChoice::Auto, "auto"),
            (ToolChoice::None, "none"),
            (ToolChoice::Required, "required"),
        ] {
            let config = RequestConfig {
                tool_choice: Some(choice),
                ..Default::default()
            };
            assert_eq!(
                build_request_body(&history, &config).unwrap()["tool_choice"],
                expected
            );
        }
        let body = build_request_body(&history, &RequestConfig::default()).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn media_and_function_results_use_typed_content() {
        let content = vec![
            text("result"),
            ContentBlock::Image(ImageBlock {
                source: MediaSource::Inline {
                    mime_type: "image/png".into(),
                    data: vec![0, 1, 2],
                },
                detail: Some(ImageDetail::High),
            }),
            ContentBlock::Image(ImageBlock {
                source: MediaSource::ProviderFile {
                    provider: PROVIDER,
                    file_id: "file-image".into(),
                },
                detail: None,
            }),
            ContentBlock::File(FileBlock {
                source: MediaSource::Url("https://example.test/file.pdf".into()),
                filename: Some("file.pdf".into()),
                mime_type: None,
            }),
            ContentBlock::File(FileBlock {
                source: MediaSource::Inline {
                    mime_type: "application/pdf".into(),
                    data: vec![3, 4],
                },
                filename: Some("inline.pdf".into()),
                mime_type: Some("application/pdf".into()),
            }),
            ContentBlock::File(FileBlock {
                source: MediaSource::ProviderFile {
                    provider: PROVIDER,
                    file_id: "file-pdf".into(),
                },
                filename: None,
                mime_type: None,
            }),
        ];
        let value = serialize_item(&item(HistoryItemKind::ToolResult(ToolResult {
            call_id: "call".into(),
            content,
            is_error: true,
        })))
        .unwrap();
        assert_eq!(value["type"], "function_call_output");
        assert_eq!(value["output"][0]["type"], "input_text");
        assert_eq!(
            value["output"][1]["image_url"],
            "data:image/png;base64,AAEC"
        );
        assert_eq!(value["output"][1]["detail"], "high");
        assert_eq!(value["output"][2]["file_id"], "file-image");
        assert_eq!(
            value["output"][3]["file_url"],
            "https://example.test/file.pdf"
        );
        assert_eq!(
            value["output"][4]["file_data"],
            "data:application/pdf;base64,AwQ="
        );
        assert_eq!(value["output"][5]["file_id"], "file-pdf");
        let image = ContentBlock::Image(ImageBlock {
            source: MediaSource::Url("https://example.test/image.png".into()),
            detail: Some(ImageDetail::Low),
        });
        assert_eq!(
            serialize_content(&image, false, None).unwrap()["image_url"],
            "https://example.test/image.png"
        );
    }

    #[test]
    fn unsupported_replay_is_rejected_locally() {
        let audio = ContentBlock::Audio(AudioBlock {
            source: None,
            format: None,
            transcript: Some("text".into()),
            provider_audio: None,
        });
        let mut history = AgentHistory::new(model());
        history.items = vec![item(HistoryItemKind::Message(Message {
            role: Role::User,
            content: vec![audio],
        }))];
        assert!(matches!(
            build_request_body(&history, &RequestConfig::default()),
            Err(Error::Unsupported { .. })
        ));
        let native = item(HistoryItemKind::ProviderItem(ProviderData {
            provider: ProviderType::Openrouter,
            raw: json!({"type":"search"}),
        }));
        assert!(matches!(
            serialize_item(&native),
            Err(Error::ProviderMismatch { .. })
        ));
        let native = item(HistoryItemKind::ProviderItem(ProviderData {
            provider: PROVIDER,
            raw: json!({"query":"untyped"}),
        }));
        assert!(matches!(
            serialize_item(&native),
            Err(Error::Unsupported { .. })
        ));
        let mut partial = item(HistoryItemKind::ToolCall(ToolCall {
            name: "read".into(),
            call_id: "call".into(),
            arguments: "{\"a\":".into(),
        }));
        assert_eq!(serialize_item(&partial).unwrap()["arguments"], "{\"a\":");
        partial.metadata.item_status = Some(ItemStatus::Incomplete);
        assert!(matches!(
            serialize_item(&partial),
            Err(Error::InvalidHistory { .. })
        ));
        assert!(matches!(
            serialize_content(
                &ContentBlock::Refusal(RefusalBlock { text: "no".into() }),
                false,
                None
            ),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn native_fields_annotations_and_encrypted_reasoning_replay() {
        let raw = json!({"type":"message","id":"msg","role":"assistant","status":"completed","future":"preserved","content":[{"type":"output_text","text":"hello","logprobs":[1],"annotations":[{"type":"url_citation","url":"https://example.test","start_index":0,"end_index":5}]}]});
        let item = parse_item(&raw, true).unwrap().unwrap();
        assert_eq!(serialize_item(&item).unwrap(), raw);
        let reasoning = json!({"id":"reason","type":"reasoning","summary":[{"type":"summary_text","text":"considered","future":true}],"encrypted_content":"encrypted","other":42});
        let item = parse_item(&reasoning, true).unwrap().unwrap();
        assert_eq!(serialize_item(&item).unwrap(), reasoning);
        let native = json!({"type":"web_search_call","id":"search","status":"completed","action":{"type":"search","query":"query"}});
        assert_eq!(
            serialize_item(&parse_item(&native, true).unwrap().unwrap()).unwrap(),
            native
        );
        let content = json!({"type":"future_content","data":[1,2]});
        assert_eq!(
            serialize_content(&parse_content(&content).unwrap(), true, None).unwrap(),
            content
        );
    }

    #[test]
    fn text_refusal_annotations_and_final_corrections() {
        let mut h = Harness::new();
        h.added(0, draft_message());
        h.feed(json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})).unwrap();
        h.feed(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"draft"})).unwrap();
        h.feed(json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"corrected"})).unwrap();
        h.feed(json!({"type":"response.output_text.annotation.added","output_index":0,"content_index":0,"annotation_index":0,"annotation":{"type":"url_citation","url":"https://example.test"}})).unwrap();
        h.feed(json!({"type":"response.refusal.delta","output_index":0,"content_index":1,"delta":"cannot"})).unwrap();
        h.feed(json!({"type":"response.refusal.done","output_index":0,"content_index":1,"refusal":"cannot comply"})).unwrap();
        let final_item = json!({"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"corrected","annotations":[{"type":"url_citation","url":"https://example.test"}]},{"type":"refusal","refusal":"cannot comply"}]});
        h.done(0, final_item.clone());
        h.finish(json!([final_item]));
        let HistoryItemKind::Message(message) = &h.output.items[0].kind else {
            panic!()
        };
        let ContentBlock::Text(text) = &message.content[0] else {
            panic!()
        };
        assert_eq!(text.text, "corrected");
        assert_eq!(text.annotations.len(), 1);
        let ContentBlock::Refusal(refusal) = &message.content[1] else {
            panic!()
        };
        assert_eq!(refusal.text, "cannot comply");
        assert!(h.events.iter().any(|e| matches!(
            e,
            TurnEvent::ItemUpdated {
                update: ItemUpdate::ContentDelta {
                    delta: ContentDelta::Refusal(_),
                    ..
                },
                ..
            }
        )));
        assert_eq!(
            h.events
                .iter()
                .filter(|e| matches!(e, TurnEvent::ItemCompleted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn reasoning_summary_and_opaque_final_state() {
        let mut h = Harness::new();
        h.added(0, json!({"id":"reason","type":"reasoning","summary":[]}));
        h.feed(json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}})).unwrap();
        h.feed(json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"draft"})).unwrap();
        h.feed(json!({"type":"response.reasoning_summary_text.done","output_index":0,"summary_index":0,"text":"final summary"})).unwrap();
        h.feed(json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"second"})).unwrap();
        h.finish(json!([{"id":"reason","type":"reasoning","summary":[{"type":"summary_text","text":"final summary"},{"type":"summary_text","text":"second"}],"encrypted_content":"opaque"}]));
        let HistoryItemKind::Reasoning(reasoning) = &h.output.items[0].kind else {
            panic!()
        };
        assert_eq!(reasoning.summary, ["final summary", "second"]);
        assert_eq!(reasoning.opaque_state.as_ref().unwrap().raw, "opaque");
        assert!(h.events.iter().any(|e| matches!(
            e,
            TurnEvent::ItemUpdated {
                update: ItemUpdate::ReasoningSummaryDelta {
                    summary_index: 0,
                    ..
                },
                ..
            }
        )));
        assert_eq!(
            serialize_item(&h.output.items[0]).unwrap()["encrypted_content"],
            "opaque"
        );
    }

    #[test]
    fn late_tool_identity_buffers_arguments_and_later_items_in_order() {
        let mut h = Harness::new();
        h.added(
            0,
            json!({"id":"tool","type":"function_call","name":"","call_id":"","arguments":""}),
        );
        h.feed(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"tool","delta":"{\"path\":"})).unwrap();
        h.added(1, json!({"id":"reason","type":"reasoning","summary":[]}));
        assert!(h.output.items.is_empty());
        assert!(h.events.is_empty());
        h.feed(json!({"type":"response.function_call_arguments.delta","output_index":0,"name":"read","call_id":"call","delta":"\"file\"}"})).unwrap();
        assert_eq!(h.output.items.len(), 2);
        let HistoryItemKind::ToolCall(call) = &h.output.items[0].kind else {
            panic!()
        };
        assert_eq!(call.name, "read");
        assert_eq!(call.call_id, "call");
        assert_eq!(call.arguments, "{\"path\":\"file\"}");
        assert!(matches!(h.events[0], TurnEvent::ItemAdded { index: 0 }));
        assert!(matches!(h.events[1], TurnEvent::ItemAdded { index: 1 }));
        h.feed(
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" "}),
        )
        .unwrap();
        assert!(matches!(
            h.events.back().unwrap(),
            TurnEvent::ItemUpdated {
                index: 0,
                update: ItemUpdate::ToolArgumentsDelta { .. }
            }
        ));
        h.feed(json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"corrected\"}"})).unwrap();
        let call = json!({"id":"tool","type":"function_call","name":"read","call_id":"call","arguments":"{\"path\":\"corrected\"}"});
        h.done(0, call.clone());
        h.finish(json!([call,{"id":"reason","type":"reasoning","summary":[]}]));
        assert!(matches!(
            h.output.outcome,
            Outcome::Completed {
                reason: StopReason::ToolCalls
            }
        ));
    }

    #[test]
    fn final_response_reconciles_unfinished_items_and_adds_missing_items() {
        let mut h = Harness::new();
        h.added(0, draft_message());
        h.feed(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"draft"})).unwrap();
        h.finish(json!([
            {"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"final","annotations":[]}]},
            {"type":"web_search_call","id":"search","status":"completed","action":{"query":"q"}}
        ]));
        assert_eq!(h.output.items.len(), 2);
        assert_eq!(
            serialize_item(&h.output.items[0]).unwrap()["content"][0]["text"],
            "final"
        );
        assert!(matches!(
            h.output.items[1].kind,
            HistoryItemKind::ProviderItem(_)
        ));
        assert_eq!(
            h.events
                .iter()
                .filter(|e| matches!(e, TurnEvent::ItemCompleted { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn terminal_only_response_is_a_complete_snapshot() {
        let mut h = Harness::new();
        h.finish(json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"all at once","annotations":[]}]}]));
        assert!(matches!(h.events[0], TurnEvent::MetadataUpdated));
        assert!(matches!(h.events[1], TurnEvent::ItemAdded { index: 0 }));
        assert!(matches!(h.events[2], TurnEvent::ItemCompleted { index: 0 }));
        assert!(matches!(
            h.output.outcome,
            Outcome::Completed {
                reason: StopReason::EndTurn
            }
        ));
    }

    #[test]
    fn usage_is_snapshot_and_unreported_counts_are_preserved() {
        let mut h = Harness::new();
        h.feed(json!({"type":"response.in_progress","response":{"id":"resp","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens":2}}})).unwrap();
        h.feed(json!({"type":"response.completed","response":{"id":"resp","status":"completed","output":[],"usage":{"output_tokens":9,"total_tokens":21,"output_tokens_details":{"reasoning_tokens":5}}}})).unwrap();
        assert_eq!(h.output.usage.input_tokens, Some(12));
        assert_eq!(h.output.usage.cached_input_tokens, Some(4));
        assert_eq!(h.output.usage.output_tokens, Some(9));
        assert_eq!(h.output.usage.total_tokens, Some(21));
        assert_eq!(h.output.usage.reasoning_output_tokens, Some(5));
        assert_eq!(
            h.events
                .iter()
                .filter(|e| matches!(e, TurnEvent::UsageUpdated))
                .count(),
            2
        );
    }

    #[test]
    fn incomplete_failed_and_cancelled_are_terminal_outcomes() {
        for (kind, status, reason) in [
            ("response.incomplete", "incomplete", "max_output_tokens"),
            ("response.incomplete", "incomplete", "content_filter"),
            ("response.incomplete", "incomplete", "future_reason"),
            ("response.failed", "failed", ""),
            ("response.cancelled", "cancelled", ""),
        ] {
            let mut h = Harness::new();
            assert!(h.feed(json!({"type":kind,"response":{"id":"resp","status":status,"output":[],"incomplete_details":{"reason":reason},"error":{"code":"failed","message":"provider failure"}}})).unwrap());
            match reason {
                "max_output_tokens" => assert!(matches!(
                    h.output.outcome,
                    Outcome::Incomplete {
                        reason: StopReason::TokenLimit
                    }
                )),
                "content_filter" => assert!(matches!(
                    h.output.outcome,
                    Outcome::Incomplete {
                        reason: StopReason::ContentFilter
                    }
                )),
                "future_reason" => assert!(matches!(
                    h.output.outcome,
                    Outcome::Incomplete {
                        reason: StopReason::Other(_)
                    }
                )),
                _ if status == "failed" => assert!(matches!(
                    h.output.outcome,
                    Outcome::Failed(Error::Provider { .. })
                )),
                _ => assert!(matches!(h.output.outcome, Outcome::Cancelled)),
            }
        }
    }

    #[test]
    fn malformed_events_identity_changes_and_premature_done_error() {
        let mut h = Harness::new();
        assert!(matches!(
            h.feed(json!({"type":"error","code":"rate_limit","message":"slow down"})),
            Err(Error::Provider { .. })
        ));
        assert!(
            h.decoder
                .decode(
                    SseEvent {
                        event: None,
                        data: "[DONE]".into()
                    },
                    &mut h.output,
                    &mut h.events
                )
                .is_err()
        );
        assert!(
            h.decoder
                .decode(
                    SseEvent {
                        event: None,
                        data: "{".into()
                    },
                    &mut h.output,
                    &mut h.events
                )
                .is_err()
        );
        assert!(h.feed(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"no item"})).is_err());
        h.added(0, draft_message());
        assert!(h.feed(json!({"type":"response.output_text.delta","output_index":0,"content_index":2,"delta":"gap"})).is_err());
        assert!(h.feed(json!({"type":"response.output_text.delta","output_index":0,"item_id":"different","content_index":0,"delta":"changed"})).is_err());
        assert!(
            h.feed(
                json!({"type":"response.completed","response":{"status":"in_progress","output":[]}})
            )
            .is_err()
        );
        let mut h = Harness::new();
        assert!(h.feed(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","arguments":"{}"}})).is_err());
    }

    #[test]
    fn completed_items_are_immutable_and_terminal_metadata_is_retained() {
        let raw = json!({"id":"msg","type":"message","role":"assistant","content":[{"type":"output_text","text":"final","annotations":[]}]});
        let mut h = Harness::new();
        h.done(0, raw.clone());
        let mut enriched = raw.clone();
        enriched["extra"] = json!("provider metadata");
        h.finish(json!([enriched.clone()]));
        assert_eq!(h.output.provider_metadata["output"][0], enriched);
        assert!(
            !h.output.items[0]
                .metadata
                .provider_data
                .as_ref()
                .unwrap()
                .raw
                .as_object()
                .unwrap()
                .contains_key("extra")
        );
        let mut h = Harness::new();
        h.done(0, raw.clone());
        assert!(h.feed(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"late"})).is_err());
        let mut conflict = raw;
        conflict["content"][0]["text"] = json!("changed");
        assert!(h.feed(json!({"type":"response.completed","response":{"status":"completed","output":[conflict]}})).is_err());
    }

    #[test]
    fn event_name_fallback_and_unknown_events_are_forward_compatible() {
        let mut h = Harness::new();
        assert!(
            !h.decoder
                .decode(
                    SseEvent {
                        event: Some("response.created".into()),
                        data: json!({"response":{"id":"resp","future":true}}).to_string()
                    },
                    &mut h.output,
                    &mut h.events
                )
                .unwrap()
        );
        assert_eq!(h.output.response_id.as_deref(), Some("resp"));
        assert_eq!(h.output.provider_metadata["future"], true);
        assert!(
            !h.feed(json!({"type":"response.future_event","anything":42}))
                .unwrap()
        );
    }

    #[test]
    fn portable_and_model_converted_assistant_messages_use_easy_input_schema() {
        let raw = json!({"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"previous answer","annotations":[]}]});
        let native = parse_item(&raw, true).unwrap().unwrap();
        assert_eq!(serialize_item(&native).unwrap(), raw);
        let mut history = AgentHistory::new(model());
        let mut output = ModelResponse::new(model());
        output.outcome = Outcome::Completed {
            reason: StopReason::EndTurn,
        };
        output.items.push(native);
        history.items = output.items;
        let mut switched = model();
        switched.model.api_name = "other-model".into();
        history.update_model_config(switched).unwrap();
        let body = build_request_body(&history, &RequestConfig::default()).unwrap();
        assert_eq!(
            body["input"][0],
            json!({"type":"message","role":"assistant","content":[{"type":"input_text","text":"previous answer"}]})
        );
        let refusal = item(HistoryItemKind::Message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Refusal(RefusalBlock { text: "no".into() })],
        }));
        assert_eq!(
            serialize_item(&refusal).unwrap()["content"][0],
            json!({"type":"input_text","text":"no"})
        );
    }

    #[test]
    fn reasoning_replay_requires_real_id_and_encrypted_state() {
        let mut reasoning = item(HistoryItemKind::Reasoning(Reasoning {
            summary: vec!["summary".into()],
            opaque_state: None,
        }));
        assert!(matches!(
            serialize_item(&reasoning),
            Err(Error::Unsupported { .. })
        ));
        reasoning.metadata.item_id = Some("reason".into());
        assert!(matches!(
            serialize_item(&reasoning),
            Err(Error::Unsupported { .. })
        ));
        let HistoryItemKind::Reasoning(state) = &mut reasoning.kind else {
            panic!()
        };
        state.opaque_state = Some(ProviderData {
            provider: PROVIDER,
            raw: json!("encrypted"),
        });
        assert_eq!(
            serialize_item(&reasoning).unwrap()["encrypted_content"],
            "encrypted"
        );
        reasoning.metadata.item_id = None;
        assert!(matches!(
            serialize_item(&reasoning),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn typed_tool_schema_is_serialized_without_semantic_changes() {
        #[derive(schemars::JsonSchema)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Input {
            /// Omitted and null are both accepted, without requiring the key.
            maybe: Option<String>,
        }
        let mut history = AgentHistory::new(model());
        history.tools.push(ToolDefinition::for_type::<Input>(
            "typed",
            "Typed tool",
            Some(false),
        ));
        let original = serde_json::to_value(&history.tools).unwrap();
        let schema = history.tools[0].parameters.as_value().clone();
        let body = build_request_body(&history, &RequestConfig::default()).unwrap();
        assert_eq!(body["tools"][0]["parameters"], schema);
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(schema.get("required").is_none());
        history.tools[0].strict = Some(true);
        assert!(matches!(
            build_request_body(&history, &RequestConfig::default()),
            Err(Error::InvalidToolDefinition { .. })
        ));
        history.tools[0].strict = Some(false);
        assert_eq!(serde_json::to_value(&history.tools).unwrap(), original);
    }

    #[test]
    fn extended_efforts_and_invalid_schemas_are_handled_before_http() {
        let mut history = AgentHistory::new(model());
        history.config.effort = Effort::Max;
        assert_eq!(
            build_request_body(&history, &RequestConfig::default()).unwrap()["reasoning"]["effort"],
            "max"
        );
        history.config.effort = Effort::Xhigh;
        assert_eq!(
            build_request_body(&history, &RequestConfig::default()).unwrap()["reasoning"]["effort"],
            "xhigh"
        );
        history.tools.push(ToolDefinition {
            name: "invalid".into(),
            description: String::new(),
            parameters: json!({"type":"not-a-json-schema-type"}).into(),
            strict: None,
        });
        assert!(matches!(
            build_request_body(&history, &RequestConfig::default()),
            Err(Error::InvalidToolDefinition { .. })
        ));
    }

    #[test]
    fn empty_and_named_keepalives_do_not_advance_state() {
        let mut h = Harness::new();
        for event in [
            SseEvent {
                event: None,
                data: String::new(),
            },
            SseEvent {
                event: None,
                data: " \n\t ".into(),
            },
            SseEvent {
                event: Some("ping".into()),
                data: "non-JSON keepalive".into(),
            },
        ] {
            assert!(
                !h.decoder
                    .decode(event, &mut h.output, &mut h.events)
                    .unwrap()
            );
        }
        assert!(h.events.is_empty());
        assert!(h.output.items.is_empty());
        assert!(matches!(h.output.outcome, Outcome::InProgress));
    }

    #[test]
    fn responses_decoder_and_replay_use_the_actual_provider_identity() {
        let provider = ProviderType::Openrouter;
        let mut h = Harness::new();
        h.output.config.model.provider = provider;
        h.output.config.model.api_name = "vendor/responses-model".into();
        let raw = json!([
            {"id":"reason","type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"encrypted"},
            {"id":"msg","type":"message","role":"assistant","status":"completed","content":[
                {"type":"output_text","text":"answer","annotations":[{"type":"url_citation","url":"https://example.test"}]},
                {"type":"future_content","payload":42}
            ]},
            {"id":"search","type":"web_search_call","status":"completed","action":{"query":"query"}}
        ]);
        h.finish(raw.clone());
        let HistoryItemKind::Reasoning(reasoning) = &h.output.items[0].kind else {
            panic!()
        };
        assert_eq!(reasoning.opaque_state.as_ref().unwrap().provider, provider);
        let HistoryItemKind::Message(message) = &h.output.items[1].kind else {
            panic!()
        };
        let ContentBlock::Text(text) = &message.content[0] else {
            panic!()
        };
        assert_eq!(text.annotations[0].provider, provider);
        let ContentBlock::ProviderContent(content) = &message.content[1] else {
            panic!()
        };
        assert_eq!(content.provider, provider);
        let HistoryItemKind::ProviderItem(native_item) = &h.output.items[2].kind else {
            panic!()
        };
        assert_eq!(native_item.provider, provider);
        for (index, item) in h.output.items.iter().enumerate() {
            assert_eq!(super::serialize_item(provider, item).unwrap(), raw[index]);
        }
        let mut history = AgentHistory::new(h.output.config.clone());
        history.items = h.output.items;
        history.items.push(item(HistoryItemKind::Message(Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image(ImageBlock {
                    source: MediaSource::ProviderFile {
                        provider,
                        file_id: "image".into(),
                    },
                    detail: None,
                }),
                ContentBlock::File(FileBlock {
                    source: MediaSource::ProviderFile {
                        provider,
                        file_id: "file".into(),
                    },
                    filename: None,
                    mime_type: None,
                }),
            ],
        })));
        let body = build_request_body(&history, &RequestConfig::default()).unwrap();
        assert_eq!(body["model"], "vendor/responses-model");
        assert_eq!(body["input"][0], raw[0]);
        assert_eq!(body["input"][1], raw[1]);
        assert_eq!(body["input"][2], raw[2]);
        assert_eq!(body["input"][3]["content"][0]["file_id"], "image");
        assert_eq!(body["input"][3]["content"][1]["file_id"], "file");
    }

    #[test]
    fn generic_provider_rejects_foreign_replay_state_and_file_ids() {
        let provider = ProviderType::Openrouter;
        for raw in [
            json!({"id":"reason","type":"reasoning","summary":[],"encrypted_content":"encrypted"}),
            json!({"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"text","annotations":[{"type":"url_citation","url":"https://example.test"}]}]}),
            json!({"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"future_content","payload":42}]}),
            json!({"id":"search","type":"web_search_call","status":"completed"}),
        ] {
            let foreign = super::parse_item(PROVIDER, &raw, true).unwrap().unwrap();
            assert!(matches!(
                super::serialize_item(provider, &foreign),
                Err(Error::ProviderMismatch {
                    expected: ProviderType::Openrouter,
                    actual: ProviderType::OpenAiCodex
                })
            ));
        }
        for block in [
            ContentBlock::Image(ImageBlock {
                source: MediaSource::ProviderFile {
                    provider: PROVIDER,
                    file_id: "image".into(),
                },
                detail: None,
            }),
            ContentBlock::File(FileBlock {
                source: MediaSource::ProviderFile {
                    provider: PROVIDER,
                    file_id: "file".into(),
                },
                filename: None,
                mime_type: None,
            }),
        ] {
            assert!(matches!(
                super::serialize_content(provider, &block, false, None),
                Err(Error::ProviderMismatch {
                    expected: ProviderType::Openrouter,
                    actual: ProviderType::OpenAiCodex
                })
            ));
        }
    }

    #[tokio::test]
    async fn generic_client_rejects_mismatched_history_before_http() {
        let client = OpenAiResponses::new(
            "http://127.0.0.1:1/responses",
            "key",
            ProviderType::Openrouter,
        )
        .unwrap();
        assert_eq!(client.clone().provider_type(), ProviderType::Openrouter);
        let history = AgentHistory::new(model());
        let config = RequestConfig::default();
        assert!(matches!(
            client.generate(&history, &config).await,
            Err(Error::ProviderMismatch {
                expected: ProviderType::Openrouter,
                actual: ProviderType::OpenAiCodex
            })
        ));
        // Even direct generate_unvalidated calls cannot send a different provider's
        // history to this client. No HTTP listener is needed for these checks.
        assert!(matches!(
            client.generate_unvalidated(&history, &config).await,
            Err(Error::ProviderMismatch {
                expected: ProviderType::Openrouter,
                actual: ProviderType::OpenAiCodex
            })
        ));
    }

    #[test]
    fn constructor_validation_and_clone() {
        assert!(OpenAiResponses::new("not a URL", "key", PROVIDER).is_err());
        assert!(
            OpenAiResponses::new("https://example.test/full-endpoint", "bad\nkey", PROVIDER)
                .is_err()
        );
        let client = OpenAiResponses::new(
            "https://example.test/full-endpoint?version=1",
            "key",
            PROVIDER,
        )
        .unwrap();
        assert_eq!(client.clone().provider_type(), PROVIDER);
    }

    #[test]
    fn resolves_all_efforts_without_changing_the_requested_config() {
        let history = AgentHistory::new(ModelConfig {
            model: Model::new(ProviderType::OpenAiCodex, "test").unwrap(),
            effort: Effort::Minimal,
        });
        for (effort, wire) in [
            (Effort::Off, "none"),
            (Effort::Minimal, "minimal"),
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            let config = RequestConfig {
                reasoning: Some(ReasoningOverride::Effort(effort)),
                ..Default::default()
            };
            let body = build_request_body(&history, &config).unwrap();
            assert_eq!(body["reasoning"]["effort"], wire);
            assert_eq!(history.config.effort, Effort::Minimal);
        }
        let body = build_request_body(
            &history,
            &RequestConfig {
                reasoning: Some(ReasoningOverride::Omit),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(body.get("reasoning").is_none());
    }
}
