//! Claude Code: `<project>/<session>.jsonl` plus `<session>/subagents/agent-<id>.jsonl`
//! with `.meta.json` sidecars. Records form a tree through `parentUuid`; the active
//! branch is the chain ending at the last conversation record.

use super::{
    ImportError, add_base64_image, collect_jsonl_files, join_block_text, parse_timestamp,
    read_jsonl, read_text, str_field, string_field, u64_field,
};
use crate::model::{
    AgentMeta, AgentTrace, Event, EventKind, FinishOutcome, Inference, SessionTrace, Source,
    ToolDefinition, ToolResult, attach_tool_result,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;

const MAIN_AGENT: &str = "main";
const CONVERSATION_TYPES: [&str; 4] = ["user", "assistant", "attachment", "system"];

pub(super) async fn import(path: &Path) -> Result<SessionTrace, ImportError> {
    let main_path = main_log_path(path);
    let records = read_jsonl(&main_path).await?;
    let session_id = records
        .iter()
        .find_map(|record| string_field(record, "sessionId"))
        .unwrap_or_else(|| file_stem(&main_path));

    let mut trace = SessionTrace::new(
        session_id,
        Source {
            harness: "claude-code".into(),
            version: records.iter().find_map(|r| string_field(r, "version")),
            path: Some(main_path.display().to_string()),
        },
    );
    trace.meta.cwd = records.iter().find_map(|r| string_field(r, "cwd"));
    trace.meta.title = records.iter().rev().find_map(|r| {
        (str_field(r, "type") == Some("ai-title")).then(|| string_field(r, "aiTitle"))?
    });

    let session_dir = main_path.with_extension("");
    let (main, mut calls) = convert_agent(&mut trace, AgentMeta::new(MAIN_AGENT), &records).await;
    trace.agents.push(main);

    let subagent_dir = session_dir.join("subagents");
    let subagent_files = match tokio::fs::try_exists(&subagent_dir).await {
        Ok(true) => collect_jsonl_files(&subagent_dir, 0).await?,
        _ => Vec::new(),
    };
    for file in subagent_files {
        let id = file_stem(&file)
            .strip_prefix("agent-")
            .map(str::to_owned)
            .unwrap_or_else(|| file_stem(&file));
        let sidecar = read_sidecar(&file.with_extension("meta.json")).await;
        let mut meta = AgentMeta::new(id);
        meta.role = string_field(&sidecar, "agentType");
        if let Some(name) =
            string_field(&sidecar, "name").or_else(|| string_field(&sidecar, "description"))
        {
            meta.name = name;
        }
        let spawn_call = string_field(&sidecar, "toolUseId");

        let records = read_jsonl(&file).await?;
        let (mut agent, agent_calls) = convert_agent(&mut trace, meta, &records).await;
        let spawned_by = spawn_call.and_then(|call_id| calls.get(&call_id).copied());
        link_parent(&trace.agents, &mut agent.meta, spawned_by);
        calls.extend(agent_calls);
        trace.agents.push(agent);
    }

    // Shapes may be logged in the main file with an agent id or in the agent's own file.
    apply_request_shapes(&mut trace, &records);
    Ok(trace)
}

/// A subagent path resolves to its session's main log.
fn main_log_path(path: &Path) -> PathBuf {
    let parent = path.parent();
    let is_subagent = parent
        .and_then(Path::file_name)
        .is_some_and(|name| name == "subagents");
    match parent.and_then(Path::parent) {
        Some(session_dir) if is_subagent => session_dir.with_extension("jsonl"),
        _ => path.to_owned(),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

async fn read_sidecar(path: &Path) -> Value {
    match read_text(path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// Attach a subagent to the agent whose tool call spawned it, falling back to main.
fn link_parent(agents: &[AgentTrace], meta: &mut AgentMeta, spawned_by: Option<u64>) {
    let spawner = spawned_by.and_then(|call| {
        agents
            .iter()
            .find(|agent| agent.events.iter().any(|event| event.id == call))
    });
    let Some(spawner) = spawner else {
        meta.parent = Some(MAIN_AGENT.into());
        return;
    };
    meta.parent = Some(spawner.meta.id.clone());
    meta.spawned_by = spawned_by;
}

/// Records of the active branch, oldest first. Compaction boundaries continue through
/// `logicalParentUuid` so pre-compaction history is kept.
fn active_branch(records: &[Value]) -> Vec<&Value> {
    let is_conversation = |record: &Value| {
        str_field(record, "type").is_some_and(|kind| CONVERSATION_TYPES.contains(&kind))
            && record.get("uuid").is_some()
    };
    let by_uuid: HashMap<&str, &Value> = records
        .iter()
        .filter(|record| is_conversation(record))
        .filter_map(|record| Some((str_field(record, "uuid")?, record)))
        .collect();

    let mut chain = Vec::new();
    let mut current = records.iter().rev().find(|record| is_conversation(record));
    while let Some(record) = current {
        chain.push(record);
        let parent =
            str_field(record, "parentUuid").or_else(|| str_field(record, "logicalParentUuid"));
        current = parent.and_then(|uuid| by_uuid.get(uuid).copied());
        if chain.len() > records.len() {
            break; // Defensive: a cycle in malformed input.
        }
    }
    chain.reverse();
    chain
}

struct PendingInference {
    message_id: String,
    t: Option<OffsetDateTime>,
    inference: Inference,
}

struct Converter<'a> {
    trace: &'a mut SessionTrace,
    events: Vec<Event>,
    /// Source call id to tool call event id, for pairing results and subagents.
    calls: HashMap<String, u64>,
    model: Option<String>,
    pending: Option<PendingInference>,
}

async fn convert_agent(
    trace: &mut SessionTrace,
    mut meta: AgentMeta,
    records: &[Value],
) -> (AgentTrace, HashMap<String, u64>) {
    let mut converter = Converter {
        trace,
        events: Vec::new(),
        calls: HashMap::new(),
        model: None,
        pending: None,
    };
    for record in active_branch(records) {
        converter.apply_record(record).await;
    }
    converter.flush_inference();

    meta.model = converter.events.iter().find_map(|event| match &event.kind {
        EventKind::Inference(inference) => inference.model.clone(),
        _ => None,
    });
    let agent = AgentTrace {
        meta,
        events: converter.events,
    };
    (agent, converter.calls)
}

impl Converter<'_> {
    fn push(&mut self, t: Option<OffsetDateTime>, kind: EventKind) {
        let id = self.trace.allocate_event_id();
        self.events.push(Event { id, t, kind });
    }

    /// Push a tool call, remembering its source `call_id` so a result can find it.
    fn push_call(&mut self, t: Option<OffsetDateTime>, call_id: String, name: String, args: Value) {
        let kind = EventKind::ToolCall {
            name,
            args,
            result: None,
        };
        let id = self.trace.allocate_event_id();
        self.events.push(Event { id, t, kind });
        self.calls.insert(call_id, id);
    }

    fn attach_result(&mut self, call_id: &str, result: ToolResult) {
        if let Some(&call) = self.calls.get(call_id) {
            attach_tool_result(&mut self.events, call, result);
        }
    }

    async fn apply_record(&mut self, record: &Value) {
        let t = parse_timestamp(record.get("timestamp"));
        let kind = str_field(record, "type").unwrap_or_default();
        if kind != "assistant" {
            self.flush_inference();
        }
        match kind {
            "assistant" => self.apply_assistant(t, record),
            "user" => self.apply_user(t, record).await,
            "attachment" => self.apply_attachment(t, record),
            "system" => self.apply_system(t, record),
            _ => {}
        }
    }

    fn apply_assistant(&mut self, t: Option<OffsetDateTime>, record: &Value) {
        let Some(message) = record.get("message") else {
            return;
        };
        let message_id = string_field(message, "id").unwrap_or_default();
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.message_id != message_id)
        {
            self.flush_inference();
        }

        let model = string_field(message, "model").filter(|model| model != "<synthetic>");
        if let Some(model) = &model
            && self.model.as_ref() != Some(model)
        {
            if self.model.is_some() {
                self.push(
                    t,
                    EventKind::ModelChange {
                        model: Some(model.clone()),
                        effort: None,
                    },
                );
            }
            self.model = Some(model.clone());
        }

        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match str_field(block, "type").unwrap_or_default() {
                "text" => {
                    let text = str_field(block, "text").unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.push(
                            t,
                            EventKind::Assistant {
                                text: text.to_owned(),
                            },
                        );
                    }
                }
                "thinking" => {
                    let text = str_field(block, "thinking").unwrap_or_default();
                    if !text.trim().is_empty() {
                        self.push(
                            t,
                            EventKind::Thinking {
                                text: text.to_owned(),
                            },
                        );
                    }
                }
                "tool_use" | "server_tool_use" => self.push_call(
                    t,
                    string_field(block, "id").unwrap_or_default(),
                    string_field(block, "name").unwrap_or_default(),
                    block.get("input").cloned().unwrap_or(Value::Null),
                ),
                _ => {}
            }
        }

        if model.is_none() {
            return;
        }
        let usage = message.get("usage").unwrap_or(&Value::Null);
        let cached = u64_field(usage, "cache_read_input_tokens");
        let cache_write = u64_field(usage, "cache_creation_input_tokens");
        let input = u64_field(usage, "input_tokens")
            .map(|input| input + cached.unwrap_or(0) + cache_write.unwrap_or(0));
        let stop_reason = string_field(message, "stop_reason")
            .or_else(|| self.pending.as_ref()?.inference.stop_reason.clone());
        self.pending = Some(PendingInference {
            message_id,
            t,
            inference: Inference {
                model,
                input_tokens: input,
                cached_input_tokens: cached,
                cache_write_tokens: cache_write,
                output_tokens: u64_field(usage, "output_tokens"),
                reasoning_tokens: usage
                    .get("output_tokens_details")
                    .and_then(|details| u64_field(details, "thinking_tokens")),
                stop_reason,
                ..Inference::default()
            },
        });
    }

    /// Claude Code logs each content block of one response as its own record with the
    /// response's full usage repeated, so usage is emitted once per message id.
    fn flush_inference(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.push(pending.t, EventKind::Inference(pending.inference));
        }
    }

    async fn apply_user(&mut self, t: Option<OffsetDateTime>, record: &Value) {
        let Some(content) = record
            .get("message")
            .and_then(|message| message.get("content"))
        else {
            return;
        };

        let mut text_parts = Vec::new();
        let mut images = Vec::new();
        match content {
            Value::String(text) => text_parts.push(text.clone()),
            Value::Array(blocks) => {
                for block in blocks {
                    match str_field(block, "type").unwrap_or_default() {
                        "text" => {
                            text_parts.push(str_field(block, "text").unwrap_or_default().to_owned())
                        }
                        "image" => images.extend(self.add_image(block)),
                        "tool_result" => self.apply_tool_result(t, record, block).await,
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        let text = text_parts.join("\n");
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        let origin = record.get("origin");
        let origin_kind = origin.and_then(|origin| str_field(origin, "kind"));
        let kind = if record.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            self.apply_compact_summary(t, text);
            return;
        } else if origin_kind == Some("peer") {
            EventKind::AgentMessage {
                from: origin
                    .and_then(|o| string_field(o, "from"))
                    .unwrap_or_default(),
                to: string_field(record, "agentId").unwrap_or_else(|| MAIN_AGENT.into()),
                text,
            }
        } else if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
            EventKind::Context {
                kind: "meta".into(),
                text,
            }
        } else if let Some(kind) = origin_kind.filter(|kind| *kind != "human") {
            EventKind::Context {
                kind: kind.into(),
                text,
            }
        } else {
            EventKind::User { text, images }
        };
        self.push(t, kind);
    }

    async fn apply_tool_result(
        &mut self,
        t: Option<OffsetDateTime>,
        record: &Value,
        block: &Value,
    ) {
        let mut images = Vec::new();
        let mut output = match block.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if str_field(part, "type") == Some("image") {
                        images.extend(self.add_image(part));
                    }
                }
                join_block_text(parts)
            }
            _ => String::new(),
        };
        if let Some(full) = read_persisted_output(&output).await {
            output = full;
        }

        let duration_ms = record.get("toolUseResult").and_then(|result| {
            u64_field(result, "durationMs").or_else(|| u64_field(result, "totalDurationMs"))
        });
        let call_id = str_field(block, "tool_use_id").unwrap_or_default();
        let result = ToolResult {
            t,
            output,
            images,
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            duration_ms,
        };
        self.attach_result(call_id, result);
    }

    fn apply_compact_summary(&mut self, t: Option<OffsetDateTime>, text: String) {
        if let Some(Event {
            kind: EventKind::Compaction { summary, .. },
            ..
        }) = self.events.last_mut()
            && summary.is_empty()
        {
            *summary = text;
            return;
        }
        self.push(
            t,
            EventKind::Compaction {
                summary: text,
                tokens_before: None,
            },
        );
    }

    fn apply_attachment(&mut self, t: Option<OffsetDateTime>, record: &Value) {
        let Some(rendered) = record.get("rendered").and_then(Value::as_array) else {
            return;
        };
        let texts: Vec<&str> = rendered
            .iter()
            .filter_map(|part| str_field(part, "content"))
            .collect();
        if texts.is_empty() {
            return;
        }
        let kind = record
            .get("attachment")
            .and_then(|attachment| string_field(attachment, "type"))
            .unwrap_or_else(|| "attachment".into());
        self.push(
            t,
            EventKind::Context {
                kind,
                text: texts.join("\n"),
            },
        );
    }

    fn apply_system(&mut self, t: Option<OffsetDateTime>, record: &Value) {
        match str_field(record, "subtype").unwrap_or_default() {
            "compact_boundary" => {
                let tokens_before = record
                    .get("compactMetadata")
                    .and_then(|metadata| u64_field(metadata, "preTokens"));
                self.push(
                    t,
                    EventKind::Compaction {
                        summary: String::new(),
                        tokens_before,
                    },
                );
            }
            "turn_duration" => self.push(
                t,
                EventKind::AgentFinish {
                    outcome: FinishOutcome::Completed,
                    reason: None,
                    duration_ms: u64_field(record, "durationMs"),
                },
            ),
            "local_command" => {
                let text = str_field(record, "content").unwrap_or_default();
                self.push(
                    t,
                    EventKind::Context {
                        kind: "local_command".into(),
                        text: text.to_owned(),
                    },
                );
            }
            _ => {}
        }
    }

    fn add_image(&mut self, block: &Value) -> Option<crate::model::ImageRef> {
        let source = block.get("source")?;
        let mime = str_field(source, "media_type").unwrap_or("image/png");
        add_base64_image(self.trace, mime, str_field(source, "data")?)
    }
}

/// Large results are replaced inline by a preview naming the file with the full output.
async fn read_persisted_output(output: &str) -> Option<String> {
    if !output.contains("<persisted-output>") {
        return None;
    }
    let path = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Full output saved to: "))?
        .trim();
    read_text(Path::new(path)).await.ok()
}

/// Request shapes record the system prompt and tool definitions sent to the model.
fn apply_request_shapes(trace: &mut SessionTrace, main_records: &[Value]) {
    let mut system_prompts: HashMap<String, String> = HashMap::new();
    for record in main_records {
        let agent = string_field(record, "agentId").unwrap_or_else(|| MAIN_AGENT.into());
        match str_field(record, "type") {
            Some("api-request-shape") => {
                let Some(shape) = record.get("shape") else {
                    continue;
                };
                for tool in shape
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add_tool_definition(trace, &agent, tool);
                }
                let system = join_block_text(
                    shape
                        .get("system")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
                if !system.is_empty() {
                    system_prompts.entry(agent).or_insert(system);
                }
            }
            Some("attachment") => {
                let Some(attachment) = record.get("attachment") else {
                    continue;
                };
                let surfaced = attachment
                    .get("surfacedDefinitions")
                    .and_then(Value::as_array);
                for entry in surfaced.into_iter().flatten() {
                    if let Some(definition) = entry.get("definition") {
                        add_tool_definition(trace, &agent, definition);
                    }
                }
            }
            _ => {}
        }
    }

    for agent in &mut trace.agents {
        if let Some(text) = system_prompts.remove(&agent.meta.id) {
            let t = agent.events.first().and_then(|event| event.t);
            agent
                .events
                .insert(0, Event::new(t, EventKind::System { text }));
        }
    }
}

fn add_tool_definition(trace: &mut SessionTrace, agent_id: &str, tool: &Value) {
    let Some(name) = string_field(tool, "name") else {
        return;
    };
    trace
        .meta
        .tools
        .entry(name.clone())
        .or_insert_with(|| ToolDefinition {
            description: string_field(tool, "description"),
            input_schema: tool.get("input_schema").cloned(),
            input_format: None,
        });
    if let Some(agent) = trace
        .agents
        .iter_mut()
        .find(|agent| agent.meta.id == agent_id)
        && !agent.meta.tools.contains(&name)
    {
        agent.meta.tools.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::assert_tool_output;
    use serde_json::json;

    fn record(value: Value) -> String {
        serde_json::to_string(&value).unwrap()
    }

    #[tokio::test]
    async fn imports_active_branch_with_merged_usage_and_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        let usage = json!({"input_tokens": 2, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5, "output_tokens": 7});
        let lines = [
            record(json!({"type": "ai-title", "aiTitle": "Title", "sessionId": "s1"})),
            record(
                json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": "s1", "cwd": "/w", "version": "2.1",
                "timestamp": "2026-11-08T17:05:37Z", "message": {"role": "user", "content": "hello"}}),
            ),
            // An abandoned branch from u1.
            record(
                json!({"type": "user", "uuid": "dead", "parentUuid": "u1", "message": {"role": "user", "content": "rewound"}}),
            ),
            record(
                json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "message": {"id": "m1", "model": "claude-x",
                "content": [{"type": "thinking", "thinking": "hmm"}], "usage": usage, "stop_reason": null}}),
            ),
            record(
                json!({"type": "assistant", "uuid": "a2", "parentUuid": "a1", "message": {"id": "m1", "model": "claude-x",
                "content": [{"type": "tool_use", "id": "t1", "name": "Agent", "input": {"prompt": "go"}}], "usage": usage, "stop_reason": "tool_use"}}),
            ),
            record(
                json!({"type": "user", "uuid": "u2", "parentUuid": "a2", "toolUseResult": {"agentId": "abc", "totalDurationMs": 9},
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "done"}]}]}}),
            ),
            record(
                json!({"type": "attachment", "uuid": "x1", "parentUuid": "u2", "attachment": {"type": "date"},
                "rendered": [{"content": "Today is X"}]}),
            ),
            record(
                json!({"type": "system", "uuid": "x2", "parentUuid": "x1", "subtype": "turn_duration", "durationMs": 100}),
            ),
            record(
                json!({"type": "api-request-shape", "shape": {"system": [{"type": "text", "text": "You are"}],
                "tools": [{"name": "Agent", "description": "spawn", "input_schema": {"type": "object"}}]}}),
            ),
        ];
        std::fs::write(&main, lines.join("\n")).unwrap();

        let subagents = dir.path().join("s1/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-abc.jsonl"),
            record(json!({"type": "user", "uuid": "s1", "parentUuid": null, "message": {"role": "user", "content": "task"}})),
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-abc.meta.json"),
            record(
                json!({"agentType": "general-purpose", "description": "Review", "toolUseId": "t1"}),
            ),
        )
        .unwrap();

        let trace = super::super::import_session(
            super::super::Harness::ClaudeCode,
            &subagents.join("agent-abc.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(trace.meta.id, "s1");
        assert_eq!(trace.meta.title.as_deref(), Some("Title"));
        assert_eq!(trace.meta.cwd.as_deref(), Some("/w"));

        let main = &trace.agents[0];
        let types: Vec<_> = main.events.iter().map(|e| e.kind.type_name()).collect();
        assert_eq!(
            types,
            [
                "system",
                "user",
                "thinking",
                "tool_call",
                "inference",
                "context",
                "agent_finish"
            ]
        );
        let EventKind::Inference(inference) = &main.events[4].kind else {
            panic!()
        };
        assert_eq!(inference.input_tokens, Some(17));
        assert_eq!(inference.stop_reason.as_deref(), Some("tool_use"));
        assert_tool_output(&main.events[3], "done");
        let ids: Vec<u64> = trace
            .agents
            .iter()
            .flat_map(|agent| agent.events.iter().map(|event| event.id))
            .collect();
        assert_eq!(ids, (0..ids.len() as u64).collect::<Vec<_>>());
        assert_eq!(main.meta.tools, ["Agent"]);

        let sub = &trace.agents[1];
        assert_eq!(sub.meta.id, "abc");
        assert_eq!(sub.meta.name, "Review");
        assert_eq!(sub.meta.parent.as_deref(), Some("main"));
        assert_eq!(sub.meta.spawned_by, Some(main.events[3].id));
    }
}
