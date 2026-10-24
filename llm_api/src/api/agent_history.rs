//! Editable history, model switching, and provider-independent request validation.

use std::collections::HashSet;

use crate::Error;
use crate::providers::{ModelConfig, ProviderType};

use super::{
    ContentBlock, HistoryItem, HistoryItemKind, HistoryItemMetadata, MediaSource, RequestConfig,
    ToolCall, ToolChoice, ToolDefinition,
};

/// Ordered model-facing context and tools available to the next request.
/// Includes all messages and tool results to send with `ApiProvider::generate`.
#[derive(Debug, Clone)]
pub struct AgentHistory {
    /// Active replay configuration, not the attribution of historical outputs.
    pub config: ModelConfig,
    pub items: Vec<HistoryItem>,
    pub tools: Vec<ToolDefinition>,
}

impl AgentHistory {
    pub fn new(config: ModelConfig) -> Self {
        Self {
            config,
            items: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Prepare history for another model/provider. The change is atomic on error.
    ///
    /// Changing model/provider explicitly discards reasoning, native provider
    /// items/content, citations, and item replay metadata. Portable messages,
    /// tool calls/results, and inline/URL media remain. Effort-only changes do
    /// not discard replay state. Response attribution and usage belong to the
    /// caller's audit log rather than this model-facing context.
    ///
    /// Provider file IDs cannot be translated: cross-provider files fail rather
    /// than silently dropping attachments. Audio IDs require an alternate source.
    /// Callers must re-upload/replace such media before retrying. A tool result
    /// whose entire content would be lost also fails instead of fabricating data.
    pub fn update_model_config(&mut self, config: ModelConfig) -> Result<(), Error> {
        let changed = self.config.model != config.model;
        let mut converted = self.clone();

        if changed {
            strip_items_replay_state(&mut converted.items, config.model.provider)?;
        }

        validate_item_providers(&converted.items, config.model.provider)?;
        converted.config = config;
        *self = converted;

        Ok(())
    }
}

/// Common preflight checks, reusable without a provider instance.
/// Adapters must additionally validate supported modalities/options and their
/// complete JSON Schema dialect/restrictions. This does not modify history.
pub fn validate_request(
    history: &AgentHistory,
    provider: ProviderType,
    config: &RequestConfig,
) -> Result<(), Error> {
    require_provider(provider, history.config.model.provider)?;
    history.config.model.validate()?;
    if config.max_output_tokens == Some(0) {
        return Err(Error::InvalidConfig {
            field: "max_output_tokens".into(),
            message: "must be greater than zero".into(),
        });
    }

    validate_tools(&history.tools, config.tool_choice.as_ref())?;
    validate_history_items(&history.items, provider)
}

/// Convert request/context items using the same policy as stored history.
/// Returns a converted copy; input remains unchanged on failure.
pub fn convert_items_for_model(
    items: &[HistoryItem],
    previous: &ModelConfig,
    config: &ModelConfig,
) -> Result<Vec<HistoryItem>, Error> {
    let mut converted = items.to_vec();
    if previous.model != config.model {
        strip_items_replay_state(&mut converted, config.model.provider)?;
    }
    validate_item_providers(&converted, config.model.provider)?;
    Ok(converted)
}

fn validate_tools(tools: &[ToolDefinition], choice: Option<&ToolChoice>) -> Result<(), Error> {
    let mut names = HashSet::new();
    for tool in tools {
        if tool.name.trim().is_empty() || tool.name.trim() != tool.name {
            return Err(Error::InvalidToolDefinition {
                name: tool.name.clone(),
                message: "name must be nonempty without surrounding whitespace".into(),
            });
        }
        if !names.insert(tool.name.as_str()) {
            return Err(Error::DuplicateToolName {
                name: tool.name.clone(),
            });
        }

        // Check structural errors without restricting valid schemas using $ref,
        // allOf, etc. Full dialect/model validation remains provider-specific.
        super::schema::validate_tool(tool)?;
    }

    match choice {
        Some(ToolChoice::Function { name }) if !names.contains(name.as_str()) => {
            return Err(Error::UnknownTool { name: name.clone() });
        }
        Some(ToolChoice::Required) if names.is_empty() => {
            return Err(Error::InvalidConfig {
                field: "tool_choice".into(),
                message: "required needs at least one tool".into(),
            });
        }
        _ => {}
    }

    Ok(())
}

fn validate_history_items(items: &[HistoryItem], provider: ProviderType) -> Result<(), Error> {
    let mut calls = ToolCalls::default();
    validate_item_providers(items, provider)?;
    calls.apply_items(items)?;

    calls.require_resolved()
}

fn require_provider(expected: ProviderType, actual: ProviderType) -> Result<(), Error> {
    if expected != actual {
        return Err(Error::ProviderMismatch { expected, actual });
    }
    Ok(())
}

fn invalid_history(message: impl Into<String>) -> Error {
    Error::InvalidHistory {
        message: message.into(),
    }
}

#[derive(Default)]
struct ToolCalls {
    seen: HashSet<String>,
    pending: HashSet<String>,
}

impl ToolCalls {
    fn apply_items(&mut self, items: &[HistoryItem]) -> Result<(), Error> {
        for item in items {
            match &item.kind {
                HistoryItemKind::ToolCall(call) => self.record_call(call, &item.metadata)?,
                HistoryItemKind::ToolResult(result) if !self.pending.remove(&result.call_id) => {
                    return Err(invalid_history(format!(
                        "unmatched or duplicate tool result {}",
                        result.call_id
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn record_call(
        &mut self,
        call: &ToolCall,
        metadata: &HistoryItemMetadata,
    ) -> Result<(), Error> {
        if call.call_id.is_empty() || call.name.trim().is_empty() {
            return Err(invalid_history("tool calls require a name and call ID"));
        }
        if matches!(
            metadata.item_status.as_deref(),
            Some("in_progress" | "incomplete")
        ) {
            return Err(invalid_history("cannot replay an unfinished tool call"));
        }

        if !self.seen.insert(call.call_id.clone()) {
            return Err(invalid_history(format!(
                "duplicate tool call ID {}",
                call.call_id
            )));
        }
        self.pending.insert(call.call_id.clone());

        Ok(())
    }

    fn require_resolved(&self) -> Result<(), Error> {
        if !self.pending.is_empty() {
            return Err(invalid_history(
                "all pending tool calls need results before another model response",
            ));
        }
        Ok(())
    }
}

fn validate_media_provider(source: &MediaSource, provider: ProviderType) -> Result<(), Error> {
    if let MediaSource::ProviderFile {
        provider: actual, ..
    } = source
    {
        require_provider(provider, *actual)?;
    }
    Ok(())
}

fn validate_content_providers(
    content: &[ContentBlock],
    provider: ProviderType,
) -> Result<(), Error> {
    for block in content {
        match block {
            ContentBlock::Image(image) => validate_media_provider(&image.source, provider)?,
            ContentBlock::File(file) => validate_media_provider(&file.source, provider)?,
            ContentBlock::Audio(audio) => {
                if let Some(source) = &audio.source {
                    validate_media_provider(source, provider)?;
                }
                if let Some(state) = &audio.provider_audio {
                    require_provider(provider, state.provider)?;
                }
            }
            ContentBlock::ProviderContent(native) => require_provider(provider, native.provider)?,
            ContentBlock::Text(text) => {
                for annotation in &text.annotations {
                    require_provider(provider, annotation.provider)?;
                }
            }
            ContentBlock::Refusal(_) => {}
        }
    }
    Ok(())
}

fn validate_item_providers(items: &[HistoryItem], provider: ProviderType) -> Result<(), Error> {
    for item in items {
        match &item.kind {
            HistoryItemKind::Message(message) => {
                validate_content_providers(&message.content, provider)?
            }
            HistoryItemKind::ToolResult(result) => {
                validate_content_providers(&result.content, provider)?
            }
            HistoryItemKind::ProviderItem(native) => require_provider(provider, native.provider)?,
            HistoryItemKind::Reasoning(reasoning) => {
                if let Some(state) = &reasoning.opaque_state {
                    require_provider(provider, state.provider)?;
                }
            }
            HistoryItemKind::ToolCall(_) => {}
        }
    }
    Ok(())
}

fn strip_content_replay_state(
    content: &mut Vec<ContentBlock>,
    provider: ProviderType,
) -> Result<(), Error> {
    content.retain(|block| !matches!(block, ContentBlock::ProviderContent(_)));

    for block in content {
        match block {
            ContentBlock::Text(text) => text.annotations.clear(),
            ContentBlock::Audio(audio) => {
                if audio.provider_audio.is_some() && audio.source.is_none() {
                    return Err(Error::Unsupported { feature: "model switching with provider-only audio; supply a portable audio source first".into() });
                }
                audio.provider_audio = None;
            }
            _ => {}
        }
        validate_content_providers(std::slice::from_ref(block), provider)?;
    }
    Ok(())
}

fn strip_items_replay_state(
    items: &mut Vec<HistoryItem>,
    provider: ProviderType,
) -> Result<(), Error> {
    items.retain(|item| {
        !matches!(
            item.kind,
            HistoryItemKind::Reasoning(_) | HistoryItemKind::ProviderItem(_)
        )
    });

    for item in items.iter_mut() {
        strip_item_replay_state(item, provider)?;
    }

    items.retain(|item| !matches!(&item.kind, HistoryItemKind::Message(message) if message.content.is_empty()));
    Ok(())
}

fn strip_item_replay_state(item: &mut HistoryItem, provider: ProviderType) -> Result<(), Error> {
    match &mut item.kind {
        HistoryItemKind::Message(message) => {
            strip_content_replay_state(&mut message.content, provider)?
        }
        HistoryItemKind::ToolResult(result) => {
            let was_empty = result.content.is_empty();
            strip_content_replay_state(&mut result.content, provider)?;
            if !was_empty && result.content.is_empty() {
                return Err(Error::Unsupported {
                    feature: "model switching would erase an entire tool result".into(),
                });
            }
        }
        _ => {}
    }

    // An incomplete native call must not become valid by losing its status.
    if matches!(item.kind, HistoryItemKind::ToolCall(_))
        && matches!(
            item.metadata.item_status.as_deref(),
            Some("in_progress" | "incomplete")
        )
    {
        return Err(invalid_history("cannot convert an unfinished tool call"));
    }
    item.metadata = HistoryItemMetadata::default();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        FileBlock, Message, ModelResponse, Outcome, ProviderOpaqueData, Reasoning, Role,
        StopReason, TextBlock, ToolCall, ToolResult,
    };
    use crate::providers::{Effort, Model};

    fn config(provider: ProviderType) -> ModelConfig {
        ModelConfig {
            model: Model::new(
                provider,
                match provider {
                    ProviderType::OpenAiCodex => "gpt-5",
                    ProviderType::Openrouter => "anthropic/claude-sonnet-4",
                },
            )
            .unwrap(),
            effort: Effort::High,
        }
    }

    fn item(kind: HistoryItemKind) -> HistoryItem {
        HistoryItem {
            kind,
            metadata: HistoryItemMetadata::default(),
        }
    }

    fn text() -> ContentBlock {
        ContentBlock::Text(TextBlock {
            text: "hello".into(),
            annotations: Vec::new(),
        })
    }

    fn output(config: ModelConfig, items: Vec<HistoryItem>) -> ModelResponse {
        let mut output = ModelResponse::default_for_model(config);
        output.outcome = Outcome::Completed {
            reason: StopReason::EndTurn,
        };
        output.items = items;
        output
    }

    #[test]
    fn accepts_empty_history_and_prompt_items() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        validate_request(&history, config.model.provider, &RequestConfig::default()).unwrap();
        assert!(history.items.is_empty());
        history.items.push(item(HistoryItemKind::Message(Message {
            role: Role::User,
            content: vec![text()],
        })));
        validate_request(&history, config.model.provider, &RequestConfig::default()).unwrap();
        assert_eq!(history.items.len(), 1);
    }

    #[test]
    fn accepts_context_without_a_response() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        history.items.push(item(HistoryItemKind::Message(Message {
            role: Role::Assistant,
            content: vec![text()],
        })));
        let request = RequestConfig::default();

        validate_request(&history, config.model.provider, &request).unwrap();
    }

    #[test]
    fn rejects_mismatched_provider_and_invalid_model() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config);
        let request = RequestConfig::default();
        assert!(matches!(
            validate_request(&history, ProviderType::Openrouter, &request),
            Err(Error::ProviderMismatch { .. })
        ));
        history.config.model.api_name = " ".into();
        assert!(matches!(
            validate_request(&history, ProviderType::OpenAiCodex, &request),
            Err(Error::InvalidConfig { .. })
        ));
    }

    #[test]
    fn checks_tool_definitions_and_choice() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        let mut request = RequestConfig {
            tool_choice: Some(ToolChoice::Required),
            ..Default::default()
        };
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::InvalidConfig { .. })
        ));
        request.tool_choice = Some(ToolChoice::Function {
            name: "read".into(),
        });
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::UnknownTool { .. })
        ));
        history.tools.push(ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type":"object"}).into(),
            strict: None,
        });
        validate_request(&history, config.model.provider, &request).unwrap();
        history.tools.push(history.tools[0].clone());
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::DuplicateToolName { .. })
        ));
        history.tools.pop();
        history.tools[0].parameters = serde_json::json!(42).into();
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::InvalidToolDefinition { .. })
        ));
        history.tools.clear();
        request.tool_choice = None;
        request.max_output_tokens = Some(0);
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::InvalidConfig { .. })
        ));
    }

    #[test]
    fn matches_tool_results_in_flat_history() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        history.items.push(item(HistoryItemKind::ToolCall(ToolCall {
            call_id: "call_1".into(),
            name: "historical_tool".into(),
            arguments: "{}".into(),
        })));
        let request = RequestConfig::default();
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::InvalidHistory { .. })
        ));
        history
            .items
            .push(item(HistoryItemKind::ToolResult(ToolResult {
                call_id: "call_1".into(),
                content: vec![text()],
                is_error: false,
            })));
        validate_request(&history, config.model.provider, &request).unwrap();
        history.items.push(history.items[1].clone());
        assert!(matches!(
            validate_request(&history, config.model.provider, &request),
            Err(Error::InvalidHistory { .. })
        ));
    }

    #[test]
    fn switching_strips_replay_state_but_preserves_attribution() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        let mut response = output(
            old.clone(),
            vec![
                item(HistoryItemKind::Reasoning(Reasoning {
                    summary: vec!["summary".into()],
                    opaque_state: Some(ProviderOpaqueData {
                        provider: old.model.provider,
                        data: serde_json::json!("encrypted"),
                    }),
                })),
                item(HistoryItemKind::Message(Message {
                    role: Role::Assistant,
                    content: vec![text()],
                })),
            ],
        );
        response.response_id = Some("resp_1".into());
        response.items[1].metadata.item_id = Some("msg_1".into());
        history.items = response.items.clone();
        let new = config(ProviderType::Openrouter);
        history.update_model_config(new.clone()).unwrap();
        assert_eq!(history.config, new);
        assert_eq!(response.config, old);
        assert_eq!(response.response_id.as_deref(), Some("resp_1"));
        assert_eq!(response.items.len(), 2);
        assert_eq!(history.items.len(), 1);
        assert!(history.items[0].metadata.item_id.is_none());
        validate_request(&history, new.model.provider, &RequestConfig::default()).unwrap();
    }

    #[test]
    fn incompatible_files_fail_without_mutating_history() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        history.items = vec![item(HistoryItemKind::Message(Message {
            role: Role::User,
            content: vec![ContentBlock::File(FileBlock {
                source: MediaSource::ProviderFile {
                    provider: old.model.provider,
                    file_id: "file_1".into(),
                },
                filename: None,
                mime_type: None,
            })],
        }))];
        assert!(matches!(
            history.update_model_config(config(ProviderType::Openrouter)),
            Err(Error::ProviderMismatch { .. })
        ));
        assert_eq!(history.config, old);
        assert_eq!(history.items.len(), 1);
    }

    #[test]
    fn switching_provider_with_the_same_api_name_strips_replay_state() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        history
            .items
            .push(item(HistoryItemKind::Reasoning(Reasoning::default())));
        let mut new = old.clone();
        new.model.provider = ProviderType::Openrouter;

        assert!(
            convert_items_for_model(&history.items, &old, &new)
                .unwrap()
                .is_empty()
        );
        history.update_model_config(new.clone()).unwrap();
        assert_eq!(history.config, new);
        assert!(history.items.is_empty());
    }

    #[test]
    fn effort_change_preserves_reasoning() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        history
            .items
            .push(item(HistoryItemKind::Reasoning(Reasoning::default())));
        let mut new = old;
        new.effort = Effort::Low;
        history.update_model_config(new).unwrap();
        assert_eq!(history.items.len(), 1);
    }
}
