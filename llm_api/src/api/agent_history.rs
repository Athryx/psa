//! Editable history, model switching, and provider-independent request validation.

use std::collections::HashSet;

use crate::Error;
use crate::providers::{ModelConfig, ProviderType};

use super::{
    ContentBlock, HistoryItem, HistoryItemKind, HistoryItemMetadata, MediaSource, ModelTurn,
    ModelTurnInput, Outcome, RequestConfig, ToolCall, ToolChoice, ToolDefinition,
};

/// Prior input/output pairs and tools available to the next request.
/// Pass the new input separately to `ApiProvider::generate`, not as a pending turn.
#[derive(Debug, Clone)]
pub struct AgentHistory {
    /// Active replay configuration, not the attribution of historical outputs.
    pub config: ModelConfig,
    pub turns: Vec<ModelTurn>,
    pub tools: Vec<ToolDefinition>,
}

impl AgentHistory {
    pub fn new(config: ModelConfig) -> Self {
        Self {
            config,
            turns: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Prepare history for another model/provider. The change is atomic on error.
    ///
    /// Changing model/provider explicitly discards reasoning, native provider
    /// items/content, citations, and item replay metadata. Portable messages,
    /// tool calls/results, and inline/URL media remain. Effort-only changes do
    /// not discard replay state. Historical output configs, response IDs, usage,
    /// and response metadata remain audit data, not continuation instructions.
    ///
    /// Provider file IDs cannot be translated: cross-provider files fail rather
    /// than silently dropping attachments. Audio IDs require an alternate source.
    /// Callers must re-upload/replace such media before retrying. A tool result
    /// whose entire content would be lost also fails instead of fabricating data.
    pub fn update_model_config(&mut self, config: ModelConfig) -> Result<(), Error> {
        let changed = self.config.provider != config.provider || self.config.model != config.model;
        let mut converted = self.clone();

        if changed {
            for turn in &mut converted.turns {
                convert_items(&mut turn.input.items, config.provider)?;
                if let Some(output) = &mut turn.output {
                    convert_items(&mut output.items, config.provider)?;
                }
            }
        }

        for turn in &converted.turns {
            validate_provider_items(&turn.input.items, config.provider)?;
            if let Some(output) = &turn.output {
                validate_provider_items(&output.items, config.provider)?;
            }
        }
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
    input: &ModelTurnInput,
    config: &RequestConfig,
) -> Result<(), Error> {
    require_provider(provider, history.config.provider)?;
    if config.max_output_tokens == Some(0) {
        return Err(Error::InvalidConfig {
            field: "max_output_tokens".into(),
            message: "must be greater than zero".into(),
        });
    }

    validate_tools(&history.tools, config.tool_choice.as_ref())?;
    validate_turns(&history.turns, provider, input)
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

fn validate_turns(
    turns: &[ModelTurn],
    provider: ProviderType,
    input: &ModelTurnInput,
) -> Result<(), Error> {
    let mut calls = ToolCalls::default();
    for (index, turn) in turns.iter().enumerate() {
        let output = turn
            .output
            .as_ref()
            .ok_or_else(|| invalid_history(format!("turn {index} has no output")))?;
        if matches!(output.outcome, Outcome::InProgress) {
            return Err(invalid_history(format!(
                "turn {index} is still in progress"
            )));
        }

        validate_provider_items(&turn.input.items, provider)?;
        calls.scan(&turn.input.items)?;
        calls.require_resolved()?;
        validate_provider_items(&output.items, provider)?;
        calls.scan(&output.items)?;
    }

    validate_provider_items(&input.items, provider)?;
    calls.scan(&input.items)?;
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
    fn scan(&mut self, items: &[HistoryItem]) -> Result<(), Error> {
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

fn validate_source(source: &MediaSource, provider: ProviderType) -> Result<(), Error> {
    if let MediaSource::ProviderFile {
        provider: actual, ..
    } = source
    {
        require_provider(provider, *actual)?;
    }
    Ok(())
}

fn validate_content(content: &[ContentBlock], provider: ProviderType) -> Result<(), Error> {
    for block in content {
        match block {
            ContentBlock::Image(image) => validate_source(&image.source, provider)?,
            ContentBlock::File(file) => validate_source(&file.source, provider)?,
            ContentBlock::Audio(audio) => {
                if let Some(source) = &audio.source {
                    validate_source(source, provider)?;
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

fn validate_provider_items(items: &[HistoryItem], provider: ProviderType) -> Result<(), Error> {
    for item in items {
        match &item.kind {
            HistoryItemKind::Message(message) => validate_content(&message.content, provider)?,
            HistoryItemKind::ToolResult(result) => validate_content(&result.content, provider)?,
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

fn convert_content(content: &mut Vec<ContentBlock>, provider: ProviderType) -> Result<(), Error> {
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
        validate_content(std::slice::from_ref(block), provider)?;
    }
    Ok(())
}

fn convert_items(items: &mut Vec<HistoryItem>, provider: ProviderType) -> Result<(), Error> {
    items.retain(|item| {
        !matches!(
            item.kind,
            HistoryItemKind::Reasoning(_) | HistoryItemKind::ProviderItem(_)
        )
    });

    for item in items.iter_mut() {
        convert_item(item, provider)?;
    }

    items.retain(|item| !matches!(&item.kind, HistoryItemKind::Message(message) if message.content.is_empty()));
    Ok(())
}

fn convert_item(item: &mut HistoryItem, provider: ProviderType) -> Result<(), Error> {
    match &mut item.kind {
        HistoryItemKind::Message(message) => convert_content(&mut message.content, provider)?,
        HistoryItemKind::ToolResult(result) => {
            let was_empty = result.content.is_empty();
            convert_content(&mut result.content, provider)?;
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
        FileBlock, Message, ModelTurnOutput, ProviderOpaqueData, Reasoning, Role, StopReason,
        TextBlock, ToolCall, ToolResult,
    };
    use crate::providers::{Effort, Model};

    fn config(provider: ProviderType) -> ModelConfig {
        ModelConfig {
            model: Model::new(match provider {
                ProviderType::OpenAiCodex => "gpt-5",
                ProviderType::Openrouter => "anthropic/claude-sonnet-4",
            })
            .unwrap(),
            provider,
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

    fn output(config: ModelConfig, items: Vec<HistoryItem>) -> ModelTurnOutput {
        let mut output = ModelTurnOutput::default_for_model(config);
        output.outcome = Outcome::Completed {
            reason: StopReason::EndTurn,
        };
        output.items = items;
        output
    }

    #[test]
    fn accepts_empty_history_and_separate_input() {
        let config = config(ProviderType::OpenAiCodex);
        let history = AgentHistory::new(config.clone());
        let input = ModelTurnInput {
            items: vec![item(HistoryItemKind::Message(Message {
                role: Role::User,
                content: vec![text()],
            }))],
        };
        validate_request(&history, config.provider, &input, &RequestConfig::default()).unwrap();
        assert!(history.turns.is_empty());
    }

    #[test]
    fn rejects_mismatched_provider_and_invalid_model() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config);
        let request = RequestConfig::default();
        assert!(matches!(
            validate_request(
                &history,
                ProviderType::Openrouter,
                &ModelTurnInput::default(),
                &request
            ),
            Err(Error::ProviderMismatch { .. })
        ));
        history.config.model.api_name = " ".into();
        assert!(matches!(
            validate_request(
                &history,
                ProviderType::OpenAiCodex,
                &ModelTurnInput::default(),
                &request
            ),
            Err(Error::InvalidConfig { .. })
        ));
    }

    #[test]
    fn checks_tool_definitions_and_choice() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        let mut request = RequestConfig::default();
        let input = ModelTurnInput::default();
        request.tool_choice = Some(ToolChoice::Required);
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
            Err(Error::InvalidConfig { .. })
        ));
        request.tool_choice = Some(ToolChoice::Function {
            name: "read".into(),
        });
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
            Err(Error::UnknownTool { .. })
        ));
        history.tools.push(ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type":"object"}),
            strict: None,
        });
        validate_request(&history, config.provider, &input, &request).unwrap();
        history.tools.push(history.tools[0].clone());
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
            Err(Error::DuplicateToolName { .. })
        ));
        history.tools.pop();
        history.tools[0].parameters = serde_json::json!(42);
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
            Err(Error::InvalidToolDefinition { .. })
        ));
        history.tools.clear();
        request.tool_choice = None;
        request.max_output_tokens = Some(0);
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
            Err(Error::InvalidConfig { .. })
        ));
    }

    #[test]
    fn matches_tool_results_across_request_boundaries() {
        let config = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(config.clone());
        history.turns.push(ModelTurn {
            input: ModelTurnInput::default(),
            output: Some(output(
                config.clone(),
                vec![item(HistoryItemKind::ToolCall(ToolCall {
                    call_id: "call_1".into(),
                    name: "historical_tool".into(),
                    arguments: "{}".into(),
                }))],
            )),
        });
        let request = RequestConfig::default();
        assert!(matches!(
            validate_request(
                &history,
                config.provider,
                &ModelTurnInput::default(),
                &request
            ),
            Err(Error::InvalidHistory { .. })
        ));
        let mut input = ModelTurnInput {
            items: vec![item(HistoryItemKind::ToolResult(ToolResult {
                call_id: "call_1".into(),
                content: vec![text()],
                is_error: false,
            }))],
        };
        validate_request(&history, config.provider, &input, &request).unwrap();
        input.items.push(input.items[0].clone());
        assert!(matches!(
            validate_request(&history, config.provider, &input, &request),
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
                        provider: old.provider,
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
        history.turns.push(ModelTurn {
            input: ModelTurnInput::default(),
            output: Some(response),
        });
        let new = config(ProviderType::Openrouter);
        history.update_model_config(new.clone()).unwrap();
        assert_eq!(history.config, new);
        let response = history.turns[0].output.as_ref().unwrap();
        assert_eq!(response.config, old);
        assert_eq!(response.response_id.as_deref(), Some("resp_1"));
        assert_eq!(response.items.len(), 1);
        assert!(response.items[0].metadata.item_id.is_none());
        validate_request(
            &history,
            new.provider,
            &ModelTurnInput::default(),
            &RequestConfig::default(),
        )
        .unwrap();
    }

    #[test]
    fn incompatible_files_fail_without_mutating_history() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        history.turns.push(ModelTurn {
            input: ModelTurnInput {
                items: vec![item(HistoryItemKind::Message(Message {
                    role: Role::User,
                    content: vec![ContentBlock::File(FileBlock {
                        source: MediaSource::ProviderFile {
                            provider: old.provider,
                            file_id: "file_1".into(),
                        },
                        filename: None,
                        mime_type: None,
                    })],
                }))],
            },
            output: Some(output(old.clone(), Vec::new())),
        });
        assert!(matches!(
            history.update_model_config(config(ProviderType::Openrouter)),
            Err(Error::ProviderMismatch { .. })
        ));
        assert_eq!(history.config, old);
        assert_eq!(history.turns[0].input.items.len(), 1);
    }

    #[test]
    fn effort_change_preserves_reasoning() {
        let old = config(ProviderType::OpenAiCodex);
        let mut history = AgentHistory::new(old.clone());
        history.turns.push(ModelTurn {
            input: ModelTurnInput::default(),
            output: Some(output(
                old.clone(),
                vec![item(HistoryItemKind::Reasoning(Reasoning::default()))],
            )),
        });
        let mut new = old;
        new.effort = Effort::Low;
        history.update_model_config(new).unwrap();
        assert_eq!(history.turns[0].output.as_ref().unwrap().items.len(), 1);
    }
}
