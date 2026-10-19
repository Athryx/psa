//! Pure transcript projection. Replaying never runs tools or makes requests.

use super::events::*;
use crate::{agents::AgentState, tools::ToolId};
use llm_api::{
    api::{
        self, AgentHistory, HistoryItem, HistoryItemKind, HistoryItemMetadata, ModelResponse,
        RequestConfig, Role,
    },
    providers::ModelConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("expected event {expected}, got {actual}")]
    EventSequence { expected: u64, actual: u64 },
    #[error("invalid transcript: {0}")]
    InvalidTranscript(String),
    #[error("invalid model history or request: {0}")]
    Model(#[from] llm_api::Error),
    #[error(transparent)]
    Request(#[from] RequestError),
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("context has no model configuration")]
    MissingModel,
    #[error("inference result {0} is missing its closing delimiter")]
    OpenInferenceResult(InferenceId),
    #[error("request is not ready: {0}")]
    Invalid(#[from] llm_api::Error),
}

#[derive(Debug, Clone)]
struct OpenInferenceResult {
    start: InferenceResultStart,
    output: ModelResponse,
}

#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub history: AgentHistory,
    pub config: RequestConfig,
}

/// Reconstructs an agent's context from events and prepares model requests.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub status: AgentState,
    pub status_reason: Option<String>,
    pub last_event_id: EventId,
    model_config: Option<ModelConfig>,
    items: Vec<HistoryItem>,
    tools: Vec<ToolDefinition>,
    open: Option<OpenInferenceResult>,
    last_response: Option<ModelResponse>,
}

impl Default for AgentContext {
    fn default() -> Self {
        Self {
            status: AgentState::Idle,
            status_reason: None,
            last_event_id: EventId(0),
            model_config: None,
            items: Vec::new(),
            tools: Vec::new(),
            open: None,
            last_response: None,
        }
    }
}

impl AgentContext {
    pub fn replay(events: &[Event]) -> Result<Self, ProjectionError> {
        let mut state = Self::default();
        for event in events {
            state.apply_event(event)?;
        }

        Ok(state)
    }

    pub fn model_config(&self) -> Option<&ModelConfig> {
        self.model_config.as_ref()
    }

    pub fn items(&self) -> &[HistoryItem] {
        &self.items
    }

    pub fn last_response(&self) -> Option<&ModelResponse> {
        self.last_response.as_ref()
    }

    pub fn open_inference_result(&self) -> Option<&InferenceResultStart> {
        self.open.as_ref().map(|o| &o.start)
    }

    pub fn tool_ids(&self) -> impl Iterator<Item = ToolId> + '_ {
        self.tools.iter().map(|t| t.id)
    }

    pub fn build_history(&self) -> Result<AgentHistory, RequestError> {
        let config = self
            .model_config
            .as_ref()
            .ok_or(RequestError::MissingModel)?;

        Ok(AgentHistory {
            config: config.clone(),
            items: self.items.clone(),
            tools: self
                .tools
                .iter()
                .map(|tool| tool.definition.clone())
                .collect(),
        })
    }

    pub fn prepare_request(&self, config: RequestConfig) -> Result<PreparedRequest, RequestError> {
        if let Some(open) = &self.open {
            return Err(RequestError::OpenInferenceResult(open.start.id));
        }

        let history = self.build_history()?;
        api::validate_request(&history, history.config.provider, &config)?;

        Ok(PreparedRequest { history, config })
    }

    pub fn apply_event(&mut self, event: &Event) -> Result<(), ProjectionError> {
        let expected = self
            .last_event_id
            .0
            .checked_add(1)
            .ok_or_else(|| invalid("event sequence exhausted"))?;
        if event.id.0 != expected {
            return Err(ProjectionError::EventSequence {
                expected,
                actual: event.id.0,
            });
        }

        match &event.kind {
            EventType::StatusChange(change) => {
                self.status = change.new_status;
                self.status_reason = change.reason.clone();
            }
            EventType::Model(config) => self.update_model_config(config)?,
            EventType::NewContext => {
                self.model_config = None;
                self.items.clear();
                self.tools.clear();
                self.open = None;
                self.last_response = None;
            }
            EventType::Message(message) => {
                let role = match message.message_type {
                    MessageType::Human | MessageType::Harness => Role::User,
                    MessageType::Agent => Role::Assistant,
                    MessageType::System => Role::System,
                };
                if self.open.is_some() && role != Role::Assistant {
                    return Err(invalid(
                        "prompt messages cannot appear inside a result block",
                    ));
                }

                self.append_item(HistoryItem {
                    kind: HistoryItemKind::Message(api::Message {
                        role,
                        content: message.content.clone(),
                    }),
                    metadata: message.metadata.clone(),
                });
            }
            EventType::Thinking(thinking) => self.append_item(HistoryItem {
                kind: HistoryItemKind::Reasoning(thinking.contents.clone()),
                metadata: thinking.metadata.clone(),
            }),
            EventType::ProviderItem(item) => {
                if !matches!(item.kind, HistoryItemKind::ProviderItem(_)) {
                    return Err(invalid(
                        "ProviderItem event must contain a native provider item",
                    ));
                }
                self.append_item(item.clone());
            }
            EventType::ToolDefinition(tool) => {
                if self.open.is_some() {
                    return Err(invalid("cannot change tools inside a result block"));
                }

                if self
                    .tools
                    .iter()
                    .any(|t| t.id == tool.id || t.definition.name == tool.definition.name)
                {
                    return Err(invalid("duplicate tool ID or function name"));
                }

                self.tools.push(tool.clone());
            }
            EventType::ToolCall(call) => self.append_item(HistoryItem {
                kind: HistoryItemKind::ToolCall(api::ToolCall {
                    call_id: call.call_id.clone(),
                    name: call.tool_name.clone(),
                    arguments: call.arguments.clone(),
                }),
                metadata: call.metadata.clone(),
            }),
            EventType::ToolResult(result) => {
                if self.open.is_some() {
                    return Err(invalid(
                        "tool results cannot appear inside a model result block",
                    ));
                }

                self.append_item(HistoryItem {
                    kind: HistoryItemKind::ToolResult(api::ToolResult {
                        call_id: result.call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.error,
                    }),
                    metadata: result.metadata.clone(),
                });
            }
            EventType::Compaction(compaction) => {
                if self.open.is_some() {
                    return Err(invalid("cannot compact inside a result block"));
                }

                let item = match &compaction.summary {
                    CompactionSummary::Text(summary) => HistoryItem {
                        kind: HistoryItemKind::Message(api::Message {
                            role: Role::User,
                            content: vec![build_text_block(&format!(
                                "Previous context summary:\n{summary}"
                            ))],
                        }),
                        metadata: HistoryItemMetadata::default(),
                    },
                    CompactionSummary::ProviderItem(item) => {
                        if !matches!(item.kind, HistoryItemKind::ProviderItem(_)) {
                            return Err(invalid("native compaction requires a provider item"));
                        }
                        item.clone()
                    }
                };
                self.items.push(item);
            }
            EventType::Log(_) => {}
            EventType::InferenceResultStart(start) => {
                if self.open.is_some() {
                    return Err(invalid("overlapping inference result blocks"));
                }

                let history = self.build_history()?;
                if history.config != start.model_config {
                    return Err(invalid(
                        "result model snapshot differs from the active context model",
                    ));
                }
                api::validate_request(&history, history.config.provider, &start.request_config)?;

                self.open = Some(OpenInferenceResult {
                    start: start.clone(),
                    output: ModelResponse::default_for_model(start.model_config.clone()),
                });
            }
            EventType::InferenceResultEnd(end) => {
                let open = self
                    .open
                    .as_ref()
                    .ok_or_else(|| invalid("result end without a result start"))?;
                if open.start.id != end.id {
                    return Err(invalid("result end ID does not match result start"));
                }
                if matches!(end.outcome, api::Outcome::InProgress) {
                    return Err(invalid("result end must have a terminal outcome"));
                }

                let mut open = self.open.take().unwrap();
                open.output.outcome = end.outcome.clone();
                open.output.usage = end.usage.clone();
                open.output.response_id = end.response_id.clone();
                open.output.provider_metadata = end.provider_metadata.clone();
                self.items.extend(open.output.items.iter().cloned());
                self.last_response = Some(open.output);
            }
        }

        self.last_event_id = event.id;
        Ok(())
    }

    fn append_item(&mut self, item: HistoryItem) {
        if let Some(open) = &mut self.open {
            open.output.items.push(item);
            return;
        }

        self.items.push(item);
    }

    fn update_model_config(&mut self, config: &ModelConfig) -> Result<(), ProjectionError> {
        if self.open.is_some() {
            return Err(invalid("cannot change model inside a result block"));
        }

        let previous = self.model_config.as_ref().unwrap_or(config);
        let items = api::agent_history::convert_items_for_model(&self.items, previous, config)?;

        self.model_config = Some(config.clone());
        self.items = items;
        Ok(())
    }
}

fn invalid(message: &str) -> ProjectionError {
    ProjectionError::InvalidTranscript(message.into())
}

fn build_text_block(value: &str) -> api::ContentBlock {
    api::ContentBlock::Text(api::TextBlock {
        text: value.into(),
        annotations: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
