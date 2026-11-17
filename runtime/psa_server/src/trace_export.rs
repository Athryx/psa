//! Export psa sessions in the harness-neutral `agent_trace` format.
//!
//! The export is lossy by design: provider replay items, metadata, and `NewContext`
//! markers are dropped, leaving the conversation and accounting each agent saw.
//! Context rebuilt by compaction is recorded once: the copied system prompt, model,
//! and retained conversation events are skipped, leaving a single compaction event.

use crate::agents::{Agent, session_manager::RuntimeError, sessions::Sessions};
use crate::db::EventRange;
use agent_trace::{
    AgentMeta, AgentTrace, Event as TraceEvent, EventKind, FinishOutcome, ImageRef, Inference,
    SessionTrace, Source, ToolDefinition, ToolResult as TraceToolResult, attach_tool_result,
};
use llm_api::{
    api::{ContentBlock, MediaSource, Outcome, StopReason},
    providers::ModelConfig,
};
use psa_api::{
    AgentState, SessionId,
    events::{CompactionSummary, Event, EventId, EventType, MessageType},
};
use std::collections::HashMap;
use time::OffsetDateTime;

const PAGE_SIZE: u32 = 1000;

/// Load every agent of a session and convert it to a trace.
pub async fn export_session(
    sessions: &Sessions,
    session_id: SessionId,
) -> Result<SessionTrace, RuntimeError> {
    let session = sessions.get_session(session_id).await?;
    let agents = sessions.list_agents(session_id).await?;

    let mut trace = SessionTrace::new(
        session_id.0.to_string(),
        Source {
            harness: "psa".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            path: None,
        },
    );
    trace.meta.title = Some(session.name.clone()).filter(|name| !name.is_empty());
    trace.meta.cwd = Some(session.working_directory.display().to_string());
    trace.meta.started_at = Some(session.created_at);

    for agent in &agents {
        let role = sessions
            .get_agent_definition(agent.definition_id)
            .await
            .ok()
            .map(|definition| definition.name);
        let events = load_all_events(sessions, session_id, agent).await?;
        let mut converted = convert_agent(&mut trace, agent, &events, &session.model_config);
        converted.meta.role = role;
        trace.agents.push(converted);
    }

    trace.register_used_tools();
    trace.update_time_bounds();
    trace.update_event_ids();
    Ok(trace)
}

async fn load_all_events(
    sessions: &Sessions,
    session_id: SessionId,
    agent: &Agent,
) -> Result<Vec<Event>, RuntimeError> {
    let mut events = Vec::new();
    let mut after = EventId(0);
    loop {
        let range = EventRange {
            after,
            through: None,
            limit: PAGE_SIZE,
        };
        let page = sessions
            .load_agent_events(session_id, agent.id, range)
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.id;
        let complete = page.len() < PAGE_SIZE as usize;
        events.extend(page);
        if complete {
            break;
        }
    }
    Ok(events)
}

/// Convert one agent's transcript, registering tool definitions and images on `trace`.
pub fn convert_agent(
    trace: &mut SessionTrace,
    agent: &Agent,
    events: &[Event],
    default_model: &ModelConfig,
) -> AgentTrace {
    let mut meta = AgentMeta::new(agent.id.0.to_string());
    meta.name = agent.name.clone();
    meta.parent = agent.subagent_parent_id.map(|parent| parent.0.to_string());

    let mut converter = Converter {
        trace,
        agent_id: meta.id.clone(),
        events: Vec::new(),
        calls: HashMap::new(),
        tools: Vec::new(),
        inference_models: HashMap::new(),
        run_started: None,
        retained_before: None,
    };
    for event in events {
        converter.apply_event(event);
    }

    meta.model = converter
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Inference(inference) => inference.model.clone(),
            _ => None,
        })
        .or_else(|| Some(model_name(default_model)));
    meta.tools = converter.tools;
    AgentTrace {
        meta,
        events: converter.events,
    }
}

struct Converter<'a> {
    trace: &'a mut SessionTrace,
    agent_id: String,
    events: Vec<TraceEvent>,
    /// psa call id to tool call event id, for pairing results.
    calls: HashMap<String, u64>,
    tools: Vec<String>,
    /// Model attribution from each inference start, keyed by inference id.
    inference_models: HashMap<u64, String>,
    run_started: Option<OffsetDateTime>,
    /// Set by a compaction: following events stamped earlier are retained copies.
    retained_before: Option<OffsetDateTime>,
}

impl Converter<'_> {
    /// Push `kind` unless it repeats the latest system prompt or model setting, as a
    /// rebuilt context does.
    fn push(&mut self, t: OffsetDateTime, kind: EventKind) {
        if matches!(
            kind,
            EventKind::System { .. } | EventKind::ModelChange { .. }
        ) && self.is_current_setting(&kind)
        {
            return;
        }
        let id = self.trace.allocate_event_id();
        self.events.push(TraceEvent {
            id,
            t: Some(t),
            kind,
        });
    }

    fn is_current_setting(&self, kind: &EventKind) -> bool {
        let latest = self
            .events
            .iter()
            .rev()
            .find(|event| std::mem::discriminant(&event.kind) == std::mem::discriminant(kind));
        latest.is_some_and(|event| &event.kind == kind)
    }

    fn apply_event(&mut self, event: &Event) {
        let t = event.timestamp;
        // Compaction replays retained events with their original timestamps.
        if let Some(before) = self.retained_before {
            if t < before {
                return;
            }
            self.retained_before = None;
        }
        match &event.kind {
            EventType::Message(message) => {
                let (text, images) = self.content_text_and_images(&message.content);
                let kind = match message.message_type {
                    MessageType::System => EventKind::System { text },
                    MessageType::Human => EventKind::User { text, images },
                    MessageType::Agent => EventKind::Assistant { text },
                    MessageType::Harness => EventKind::Context {
                        kind: message
                            .description
                            .clone()
                            .unwrap_or_else(|| "harness".into()),
                        text,
                    },
                    MessageType::Subagent => {
                        let from = message
                            .source
                            .as_ref()
                            .map(|source| source.sender_name.clone())
                            .unwrap_or_default();
                        EventKind::AgentMessage {
                            from,
                            to: self.agent_id.clone(),
                            text,
                        }
                    }
                };
                self.push(t, kind);
            }
            EventType::Thinking(thinking) => {
                let text = thinking.contents.summary.join("\n");
                if !text.trim().is_empty() {
                    self.push(t, EventKind::Thinking { text });
                }
            }
            EventType::ToolDefinition(definition) => {
                let definition = &definition.definition;
                let name = definition.name.clone();
                self.trace
                    .meta
                    .tools
                    .entry(name.clone())
                    .or_insert_with(|| ToolDefinition {
                        description: Some(definition.description.clone()),
                        input_schema: serde_json::to_value(&definition.parameters).ok(),
                        input_format: None,
                    });
                if !self.tools.contains(&name) {
                    self.tools.push(name);
                }
            }
            EventType::ToolCall(call) => {
                let args = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
                let kind = EventKind::ToolCall {
                    name: call.tool_name.clone(),
                    args,
                    result: None,
                };
                let id = self.trace.allocate_event_id();
                self.events.push(TraceEvent {
                    id,
                    t: Some(t),
                    kind,
                });
                self.calls.insert(call.call_id.clone(), id);
            }
            EventType::ToolResult(result) => {
                let (output, images) = self.content_text_and_images(&result.content);
                let trace_result = TraceToolResult {
                    t: Some(t),
                    output,
                    images,
                    is_error: result.error,
                    duration_ms: None,
                };
                if let Some(&call) = self.calls.get(&result.call_id) {
                    attach_tool_result(&mut self.events, call, trace_result);
                }
            }
            EventType::Compaction(compaction) => {
                self.retained_before = Some(t);
                let summary = match &compaction.summary {
                    CompactionSummary::Text(text) => text.clone(),
                    CompactionSummary::ProviderItem(_) => "[provider-native compaction]".into(),
                };
                self.push(
                    t,
                    EventKind::Compaction {
                        summary,
                        tokens_before: None,
                    },
                );
            }
            EventType::Model(model) => self.push(
                t,
                EventKind::ModelChange {
                    model: Some(model_name(model)),
                    effort: Some(model.effort.as_str().to_owned()),
                },
            ),
            EventType::InferenceResultStart(start) => {
                self.inference_models
                    .insert(start.id, model_name(&start.model_config));
            }
            EventType::InferenceResultEnd(end) => {
                let (stop_reason, error) = match &end.outcome {
                    Outcome::Completed { reason } | Outcome::Incomplete { reason } => {
                        (Some(stop_reason_name(reason)), None)
                    }
                    Outcome::Failed(error) => (None, Some(error.to_string())),
                    Outcome::Cancelled => (Some("cancelled".to_owned()), None),
                    Outcome::InProgress => (None, None),
                };
                let model = self.inference_models.remove(&end.id);
                self.push(
                    t,
                    EventKind::Inference(Inference {
                        model,
                        input_tokens: end.usage.input_tokens,
                        cached_input_tokens: end.usage.cached_input_tokens,
                        cache_write_tokens: None,
                        output_tokens: end.usage.output_tokens,
                        reasoning_tokens: end.usage.reasoning_output_tokens,
                        cost_usd: None,
                        stop_reason,
                        error,
                        duration_ms: u64::try_from(end.request_time.as_millis()).ok(),
                    }),
                );
            }
            EventType::StatusChange(change) => self.apply_status(t, change),
            EventType::Result(result) => {
                let text = match result {
                    psa_api::AgentResult::Text(text) => text.clone(),
                    psa_api::AgentResult::Json(value) => value.to_string(),
                };
                self.push(
                    t,
                    EventKind::Context {
                        kind: "result".into(),
                        text,
                    },
                );
            }
            EventType::Log(log) => self.push(
                t,
                EventKind::Context {
                    kind: if log.error { "error" } else { "log" }.into(),
                    text: log.message.clone(),
                },
            ),
            EventType::Command(command) => self.push(
                t,
                EventKind::Context {
                    kind: "command".into(),
                    text: describe_command(command),
                },
            ),
            EventType::DecisionCall(call) => self.push(
                t,
                EventKind::Context {
                    kind: "decision_call".into(),
                    text: serde_json::to_string(call).unwrap_or_default(),
                },
            ),
            EventType::ProviderItem(_) | EventType::NewContext => {}
        }
    }

    /// A run ends when the agent leaves `Running`.
    fn apply_status(&mut self, t: OffsetDateTime, change: &psa_api::events::StatusChange) {
        if change.new_status == AgentState::Running {
            self.run_started = Some(t);
            return;
        }
        let Some(started) = self.run_started.take() else {
            return;
        };
        let outcome = match change.new_status {
            AgentState::Interrupted => FinishOutcome::Aborted,
            _ => FinishOutcome::Completed,
        };
        let duration_ms = u64::try_from((t - started).whole_milliseconds()).ok();
        self.push(
            t,
            EventKind::AgentFinish {
                outcome,
                reason: change.reason.clone(),
                duration_ms,
            },
        );
    }

    fn content_text_and_images(&mut self, content: &[ContentBlock]) -> (String, Vec<ImageRef>) {
        let mut texts = Vec::new();
        let mut images = Vec::new();
        for block in content {
            match block {
                ContentBlock::Text(text) => texts.push(text.text.clone()),
                ContentBlock::Image(image) => match &image.source {
                    MediaSource::Inline { mime_type, data } => {
                        images.push(self.trace.add_image(mime_type, data.clone()));
                    }
                    MediaSource::Url(url) => texts.push(format!("[image {url}]")),
                    MediaSource::ProviderFile { file_id, .. } => {
                        texts.push(format!("[image file {file_id}]"));
                    }
                },
                ContentBlock::Refusal(refusal) => {
                    texts.push(serde_json::to_string(refusal).unwrap_or_default());
                }
                ContentBlock::Audio(_) => texts.push("[audio]".into()),
                ContentBlock::File(_) => texts.push("[file]".into()),
                ContentBlock::ProviderContent(_) => {}
            }
        }
        (texts.join("\n"), images)
    }
}

fn model_name(config: &ModelConfig) -> String {
    format!(
        "{}/{}",
        config.model.provider.as_str(),
        config.model.api_name
    )
}

fn stop_reason_name(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".into(),
        StopReason::ToolCalls => "tool_calls".into(),
        StopReason::StopSequence => "stop_sequence".into(),
        StopReason::TokenLimit => "token_limit".into(),
        StopReason::ContentFilter => "content_filter".into(),
        StopReason::Other(other) => other.clone(),
    }
}

/// The command line, then the outcome when it did not succeed, then any output.
fn describe_command(command: &psa_api::events::CommandRecord) -> String {
    let mut text = command.command_line();
    if command.outcome != psa_api::CommandOutcome::Succeeded {
        text = format!("{text} ({:?})", command.outcome);
    }
    if let Some(output) = &command.output {
        text = format!("{text}\n{output}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::{
        api::{RequestConfig, TextBlock, Usage},
        providers::{Effort, Model, ProviderType},
    };
    use psa_api::{
        AgentDefinitionId, AgentId,
        events::{
            AgentMessageLink, InferenceResultEnd, InferenceResultStart, Message, StatusChange,
            ToolCall, ToolResult,
        },
    };
    use std::time::Duration;

    fn model() -> ModelConfig {
        ModelConfig {
            model: Model::new(ProviderType::Openrouter, "vendor/model").unwrap(),
            effort: Effort::High,
        }
    }

    fn agent() -> Agent {
        Agent {
            id: AgentId(7),
            session_id: SessionId(1),
            definition_id: AgentDefinitionId(1),
            parent_id: None,
            subagent_parent_id: Some(AgentId(2)),
            first_event_id: EventId(0),
            next_event_id: EventId(0),
            created_position: 0,
            name: "worker".into(),
            interactive: false,
            hidden: false,
            state: AgentState::Idle,
            usage: Default::default(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn event(id: u64, seconds: i64, kind: EventType) -> Event {
        Event {
            id: EventId(id),
            source_agent_id: AgentId(7),
            timestamp: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds),
            kind,
        }
    }

    fn text(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextBlock {
            text: text.into(),
            annotations: Vec::new(),
        })]
    }

    #[test]
    fn converts_conversation_inference_and_runs() {
        let mut subagent_message = Message::text(MessageType::Subagent, "do it");
        subagent_message.source = Some(AgentMessageLink {
            request_id: "r".into(),
            sender: AgentId(2),
            sender_name: "main".into(),
            recipient: AgentId(1),
            recipient_name: "recipient".into(),
        });
        let events = [
            event(
                1,
                0,
                EventType::StatusChange(StatusChange {
                    new_status: AgentState::Running,
                    reason: None,
                }),
            ),
            event(2, 0, EventType::Message(subagent_message)),
            event(
                3,
                1,
                EventType::InferenceResultStart(InferenceResultStart {
                    id: 9,
                    model_config: model(),
                    request_config: RequestConfig::default(),
                }),
            ),
            event(
                4,
                2,
                EventType::ToolCall(ToolCall {
                    call_id: "c1".into(),
                    tool_name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                    metadata: Default::default(),
                }),
            ),
            event(
                5,
                2,
                EventType::InferenceResultEnd(InferenceResultEnd {
                    id: 9,
                    outcome: Outcome::Completed {
                        reason: StopReason::ToolCalls,
                    },
                    usage: Usage {
                        input_tokens: Some(100),
                        cached_input_tokens: Some(60),
                        output_tokens: Some(4),
                        ..Usage::default()
                    },
                    request_time: Duration::from_millis(1500),
                    response_id: None,
                    provider_metadata: Default::default(),
                }),
            ),
            event(
                6,
                3,
                EventType::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    content: text("a.txt"),
                    error: false,
                    metadata: Default::default(),
                    delivery: None,
                }),
            ),
            event(7, 3, EventType::NewContext),
            event(
                8,
                10,
                EventType::StatusChange(StatusChange {
                    new_status: AgentState::Interrupted,
                    reason: Some("stopped".into()),
                }),
            ),
        ];

        let mut trace = SessionTrace::new("1", Source::default_for("psa"));
        let agent = convert_agent(&mut trace, &agent(), &events, &model());

        assert_eq!(agent.meta.id, "7");
        assert_eq!(agent.meta.parent.as_deref(), Some("2"));
        assert_eq!(agent.meta.model.as_deref(), Some("openrouter/vendor/model"));
        let types: Vec<_> = agent.events.iter().map(|e| e.kind.type_name()).collect();
        assert_eq!(
            types,
            ["agent_message", "tool_call", "inference", "agent_finish"]
        );
        assert!(
            matches!(&agent.events[0].kind, EventKind::AgentMessage { from, to, .. } if from == "main" && to == "7")
        );
        let EventKind::Inference(inference) = &agent.events[2].kind else {
            panic!("expected inference")
        };
        assert_eq!(inference.stop_reason.as_deref(), Some("tool_calls"));
        assert_eq!(inference.duration_ms, Some(1500));
        let EventKind::ToolCall {
            result: Some(result),
            ..
        } = &agent.events[1].kind
        else {
            panic!("expected an answered tool call")
        };
        assert_eq!(result.output, "a.txt");
        assert!(matches!(
            &agent.events[3].kind,
            EventKind::AgentFinish {
                outcome: FinishOutcome::Aborted,
                duration_ms: Some(10_000),
                ..
            }
        ));
    }

    #[test]
    fn compaction_is_recorded_once_without_rebuilt_context() {
        let human = |text: &str| EventType::Message(Message::text(MessageType::Human, text));
        let agent_text = |text: &str| EventType::Message(Message::text(MessageType::Agent, text));
        let system = || EventType::Message(Message::system("prompt", "Base instructions"));
        let events = [
            event(1, 0, EventType::NewContext),
            event(2, 0, EventType::Model(model())),
            event(3, 0, system()),
            event(4, 1, human("old")),
            event(5, 2, agent_text("kept")),
            // Compaction rebuilds the context, replaying `kept` with its original time.
            event(6, 10, EventType::NewContext),
            event(7, 10, EventType::Model(model())),
            event(8, 10, system()),
            event(
                9,
                10,
                EventType::Compaction(psa_api::events::Compaction {
                    summary: CompactionSummary::Text("summary".into()),
                    usage: Usage::default(),
                }),
            ),
            event(10, 2, agent_text("kept")),
            event(11, 11, human("new")),
        ];

        let mut trace = SessionTrace::new("1", Source::default_for("psa"));
        let agent = convert_agent(&mut trace, &agent(), &events, &model());

        let types: Vec<_> = agent.events.iter().map(|e| e.kind.type_name()).collect();
        assert_eq!(
            types,
            [
                "model_change",
                "system",
                "user",
                "assistant",
                "compaction",
                "user"
            ]
        );
        assert!(matches!(&agent.events[5].kind, EventKind::User { text, .. } if text == "new"));
    }
}
