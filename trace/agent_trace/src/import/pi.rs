//! pi: one session file per agent. Entries form a tree through `id`/`parentId`; the
//! active branch ends at the last entry. The `subagents` extension records children
//! with `subagents.member.v1` entries pointing at their own session files, and their
//! messages as `subagents.message.v1` custom messages.

use super::{
    ImportError, add_base64_image, join_block_text, parse_timestamp, read_jsonl, str_field,
    string_field, u64_field,
};
use crate::model::{
    AgentMeta, AgentTrace, Event, EventKind, ImageRef, Inference, SessionTrace, Source,
    ToolDefinition, ToolResult, attach_tool_result,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
};
use time::OffsetDateTime;

const IDENTITY: &str = "subagents.identity.v1";
const MEMBER: &str = "subagents.member.v1";
const MESSAGE: &str = "subagents.message.v1";
/// pi's system prompt sections in prompt order; unknown sections follow alphabetically.
const SECTION_ORDER: [&str; 7] = [
    "preamble",
    "tools",
    "rules",
    "docs",
    "project_context",
    "skills",
    "cwd",
];

pub(super) async fn import(path: &Path) -> Result<SessionTrace, ImportError> {
    let root_path = root_session_path(path).await?;
    let records = read_jsonl(&root_path).await?;
    let header = records
        .iter()
        .find(|record| str_field(record, "type") == Some("session"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut trace = SessionTrace::new(
        string_field(&header, "id").unwrap_or_else(|| root_path.display().to_string()),
        Source {
            harness: "pi".into(),
            version: None,
            path: Some(root_path.display().to_string()),
        },
    );
    trace.meta.cwd = string_field(&header, "cwd");
    trace.meta.started_at = parse_timestamp(header.get("timestamp"));

    let root_name = identity(&records)
        .and_then(|identity| string_field(identity, "name"))
        .unwrap_or_else(|| "main".into());
    let mut queue = VecDeque::from([(AgentMeta::new(root_name), records)]);
    let mut seen_files = HashSet::from([root_path]);
    while let Some((meta, records)) = queue.pop_front() {
        let children = members(&records);
        let mut agent = convert_agent(&mut trace, meta, &records);
        agent.meta.spawned_by = find_spawn_call(&trace.agents, &agent.meta);
        let parent_id = agent.meta.id.clone();
        trace.agents.push(agent);

        for (mut child, file) in children {
            if !seen_files.insert(file.clone()) {
                continue;
            }
            // Workers may be deleted or never started; keep the rest of the session.
            let Ok(child_records) = read_jsonl(&file).await else {
                continue;
            };
            child.parent.get_or_insert(parent_id.clone());
            queue.push_back((child, child_records));
        }
    }
    Ok(trace)
}

/// Worker files name their root session; anything else is its own root.
async fn root_session_path(path: &Path) -> Result<PathBuf, ImportError> {
    let records = read_jsonl(path).await?;
    let root = identity(&records)
        .and_then(|identity| str_field(identity, "rootSessionFile"))
        .map(PathBuf::from)
        .filter(|root| root != path);
    match root {
        Some(root) if tokio::fs::try_exists(&root).await.unwrap_or(false) => Ok(root),
        _ => Ok(path.to_owned()),
    }
}

fn identity(records: &[Value]) -> Option<&Value> {
    records
        .iter()
        .find(|record| str_field(record, "customType") == Some(IDENTITY))
        .and_then(|record| record.get("data"))
}

/// Children recorded in this file, latest entry per instance. Membership is
/// session-wide, so all entries count, not only the active branch.
fn members(records: &[Value]) -> Vec<(AgentMeta, PathBuf)> {
    let mut order = Vec::new();
    let mut latest: HashMap<String, (AgentMeta, PathBuf)> = HashMap::new();
    for record in records {
        if str_field(record, "customType") != Some(MEMBER) {
            continue;
        }
        let Some(data) = record.get("data") else {
            continue;
        };
        let identity = data.get("identity").unwrap_or(&Value::Null);
        let (Some(name), Some(file)) = (
            string_field(identity, "name"),
            string_field(data, "sessionFile"),
        ) else {
            continue;
        };
        let instance = string_field(identity, "instanceId").unwrap_or_else(|| name.clone());

        let mut meta = AgentMeta::new(name);
        meta.parent = string_field(identity, "parentName");
        meta.role = string_field(identity, "template").or_else(|| {
            data.pointer("/template/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        if !latest.contains_key(&instance) {
            order.push(instance.clone());
        }
        latest.insert(instance, (meta, PathBuf::from(file)));
    }
    order
        .into_iter()
        .filter_map(|instance| latest.remove(&instance))
        .collect()
}

/// The parent's `create_agent` call that named this agent.
fn find_spawn_call(agents: &[AgentTrace], meta: &AgentMeta) -> Option<u64> {
    let parent = agents
        .iter()
        .find(|agent| Some(&agent.meta.id) == meta.parent.as_ref())?;
    parent.events.iter().find_map(|event| match &event.kind {
        EventKind::ToolCall { name, args, .. }
            if name == "create_agent"
                && args.get("name").and_then(Value::as_str) == Some(meta.id.as_str()) =>
        {
            Some(event.id)
        }
        _ => None,
    })
}

/// Entries of the active branch, oldest first, without entries removed by `context_edit`.
fn active_branch(records: &[Value]) -> Vec<&Value> {
    let by_id: HashMap<&str, &Value> = records
        .iter()
        .filter(|record| str_field(record, "type") != Some("session"))
        .filter_map(|record| Some((str_field(record, "id")?, record)))
        .collect();

    let mut chain = Vec::new();
    let mut current = records
        .iter()
        .rev()
        .find(|record| str_field(record, "type") != Some("session") && record.get("id").is_some());
    while let Some(record) = current {
        chain.push(record);
        current = str_field(record, "parentId").and_then(|id| by_id.get(id).copied());
        if chain.len() > records.len() {
            break; // Defensive: a cycle in malformed input.
        }
    }
    chain.reverse();

    let removed: HashSet<&str> = chain
        .iter()
        .filter(|record| str_field(record, "type") == Some("context_edit"))
        .filter(|record| record.get("replacement").is_none_or(Value::is_null))
        .filter_map(|record| str_field(record, "targetId"))
        .collect();
    chain.retain(|record| !str_field(record, "id").is_some_and(|id| removed.contains(id)));
    chain
}

struct Converter<'a> {
    trace: &'a mut SessionTrace,
    agent_id: String,
    events: Vec<Event>,
    /// Source call id to tool call event id, for pairing results.
    calls: HashMap<String, u64>,
    tools: Vec<String>,
    model: Option<String>,
    effort: Option<String>,
}

fn convert_agent(trace: &mut SessionTrace, mut meta: AgentMeta, records: &[Value]) -> AgentTrace {
    let mut converter = Converter {
        trace,
        agent_id: meta.id.clone(),
        events: Vec::new(),
        calls: HashMap::new(),
        tools: Vec::new(),
        model: None,
        effort: None,
    };
    for record in active_branch(records) {
        converter.apply_entry(record);
    }

    meta.model = converter.events.iter().find_map(|event| match &event.kind {
        EventKind::Inference(inference) => inference.model.clone(),
        _ => None,
    });
    meta.model = meta.model.or(converter.model);
    meta.tools = converter.tools;
    AgentTrace {
        meta,
        events: converter.events,
    }
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

    fn has_conversation(&self) -> bool {
        self.events
            .iter()
            .any(|event| !matches!(event.kind, EventKind::System { .. }))
    }

    fn apply_entry(&mut self, entry: &Value) {
        let t = parse_timestamp(entry.get("timestamp"));
        match str_field(entry, "type").unwrap_or_default() {
            "message" => {
                if let Some(message) = entry.get("message") {
                    self.apply_message(t, message);
                }
            }
            "custom_message" => self.apply_custom_message(t, entry),
            "compaction" => self.push(
                t,
                EventKind::Compaction {
                    summary: string_field(entry, "summary").unwrap_or_default(),
                    tokens_before: u64_field(entry, "tokensBefore"),
                },
            ),
            "model_change" => {
                let model = match (str_field(entry, "provider"), str_field(entry, "modelId")) {
                    (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                    (None, model) => model.map(str::to_owned),
                    (_, None) => None,
                };
                self.apply_model_change(t, model, None);
            }
            "thinking_level_change" => {
                self.apply_model_change(t, None, string_field(entry, "thinkingLevel"));
            }
            _ => {}
        }
    }

    /// Initial selections set the starting model; later ones become events.
    fn apply_model_change(
        &mut self,
        t: Option<OffsetDateTime>,
        model: Option<String>,
        effort: Option<String>,
    ) {
        if model.is_some() {
            self.model.clone_from(&model);
        }
        if effort.is_some() {
            self.effort.clone_from(&effort);
        }
        if self.has_conversation() {
            self.push(t, EventKind::ModelChange { model, effort });
        }
    }

    fn apply_message(&mut self, t: Option<OffsetDateTime>, message: &Value) {
        match str_field(message, "role").unwrap_or_default() {
            "system" => {
                let text = system_text(message);
                self.add_tools_from_system_prompt(message);
                let repeated = self
                    .events
                    .iter()
                    .rev()
                    .find_map(|event| match &event.kind {
                        EventKind::System { text } => Some(text),
                        _ => None,
                    })
                    .is_some_and(|previous| *previous == text);
                if !text.is_empty() && !repeated {
                    self.push(t, EventKind::System { text });
                }
            }
            "user" => {
                let (text, images) = self.content_text_and_images(message.get("content"));
                if !text.trim().is_empty() || !images.is_empty() {
                    self.push(t, EventKind::User { text, images });
                }
            }
            "assistant" => self.apply_assistant(t, message),
            "toolResult" => {
                let (output, images) = self.content_text_and_images(message.get("content"));
                let call_id = str_field(message, "toolCallId").unwrap_or_default();
                let result = ToolResult {
                    t,
                    output,
                    images,
                    is_error: message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    duration_ms: u64_field(message, "durationMs"),
                };
                self.attach_result(call_id, result);
            }
            _ => {}
        }
    }

    fn apply_assistant(&mut self, t: Option<OffsetDateTime>, message: &Value) {
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
                "toolCall" => self.push_call(
                    t,
                    string_field(block, "id").unwrap_or_default(),
                    string_field(block, "name").unwrap_or_default(),
                    block.get("arguments").cloned().unwrap_or(Value::Null),
                ),
                _ => {}
            }
        }

        let usage = message.get("usage").unwrap_or(&Value::Null);
        let cached = u64_field(usage, "cacheRead");
        let cache_write = u64_field(usage, "cacheWrite");
        let model = match (str_field(message, "provider"), str_field(message, "model")) {
            (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
            (None, model) => model.map(str::to_owned),
            (_, None) => None,
        };
        self.push(
            t,
            EventKind::Inference(Inference {
                model,
                input_tokens: u64_field(usage, "input")
                    .map(|input| input + cached.unwrap_or(0) + cache_write.unwrap_or(0)),
                cached_input_tokens: cached,
                cache_write_tokens: cache_write,
                output_tokens: u64_field(usage, "output"),
                reasoning_tokens: u64_field(usage, "reasoning"),
                cost_usd: usage.pointer("/cost/total").and_then(Value::as_f64),
                stop_reason: string_field(message, "stopReason"),
                error: string_field(message, "errorMessage"),
                duration_ms: u64_field(message, "durationMs"),
            }),
        );
    }

    fn apply_custom_message(&mut self, t: Option<OffsetDateTime>, entry: &Value) {
        let custom_type = str_field(entry, "customType").unwrap_or("custom");
        if custom_type == MESSAGE
            && let Some(details) = entry.get("details")
        {
            self.push(
                t,
                EventKind::AgentMessage {
                    from: string_field(details, "from").unwrap_or_default(),
                    to: string_field(details, "to").unwrap_or_else(|| self.agent_id.clone()),
                    text: string_field(details, "message").unwrap_or_default(),
                },
            );
            return;
        }

        let (text, _) = self.content_text_and_images(entry.get("content"));
        if !text.trim().is_empty() {
            self.push(
                t,
                EventKind::Context {
                    kind: custom_type.to_owned(),
                    text,
                },
            );
        }
    }

    fn content_text_and_images(&mut self, content: Option<&Value>) -> (String, Vec<ImageRef>) {
        match content {
            Some(Value::String(text)) => (text.clone(), Vec::new()),
            Some(Value::Array(blocks)) => {
                let images = blocks
                    .iter()
                    .filter(|block| str_field(block, "type") == Some("image"))
                    .filter_map(|block| {
                        let mime = str_field(block, "mimeType").unwrap_or("image/png");
                        add_base64_image(self.trace, mime, str_field(block, "data")?)
                    })
                    .collect();
                (join_block_text(blocks), images)
            }
            _ => (String::new(), Vec::new()),
        }
    }

    /// pi does not log tool schemas; its system prompt lists `- name: description`.
    fn add_tools_from_system_prompt(&mut self, message: &Value) {
        let Some(tools) = message.pointer("/sections/tools").and_then(Value::as_str) else {
            return;
        };
        for line in tools.lines() {
            let Some((name, description)) = line
                .trim()
                .strip_prefix("- ")
                .and_then(|rest| rest.split_once(": "))
            else {
                continue;
            };
            if name.is_empty() || name.contains(char::is_whitespace) {
                continue;
            }
            self.trace
                .meta
                .tools
                .entry(name.to_owned())
                .or_insert_with(|| ToolDefinition {
                    description: Some(description.to_owned()),
                    ..ToolDefinition::default()
                });
            if !self.tools.iter().any(|tool| tool == name) {
                self.tools.push(name.to_owned());
            }
        }
    }
}

/// Plain content, or the structured sections joined in prompt order.
fn system_text(message: &Value) -> String {
    if let Some(text) = str_field(message, "content").filter(|text| !text.is_empty()) {
        return text.to_owned();
    }
    let Some(sections) = message.get("sections").and_then(Value::as_object) else {
        return String::new();
    };
    let known = SECTION_ORDER.iter().filter_map(|name| sections.get(*name));
    let rest = sections
        .iter()
        .filter(|(name, _)| !SECTION_ORDER.contains(&name.as_str()))
        .map(|(_, value)| value);
    known
        .chain(rest)
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::assert_tool_output;
    use serde_json::json;

    fn line(value: Value) -> String {
        serde_json::to_string(&value).unwrap()
    }

    #[tokio::test]
    async fn imports_branch_edits_and_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root.jsonl");
        let worker = dir.path().join("subagents/ws/worker.jsonl");
        std::fs::create_dir_all(worker.parent().unwrap()).unwrap();
        let root_file = root.display().to_string();

        let root_lines = [
            line(
                json!({"type": "session", "version": 3, "id": "sess", "timestamp": "2026-11-08T04:09:57Z", "cwd": "/w"}),
            ),
            line(
                json!({"type": "model_change", "id": "m0", "parentId": null, "provider": "openrouter", "modelId": "x/y"}),
            ),
            line(
                json!({"type": "custom", "customType": IDENTITY, "id": "i0", "parentId": "m0", "data": {"name": "main", "rootSessionFile": root_file}}),
            ),
            line(
                json!({"type": "message", "id": "s0", "parentId": "i0", "message": {"role": "system", "content": "",
                "sections": {"tools": "<tools>\n- bash: Run commands\n</tools>", "preamble": "You are pi"}}}),
            ),
            line(
                json!({"type": "message", "id": "u1", "parentId": "s0", "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
            ),
            line(
                json!({"type": "message", "id": "a1", "parentId": "u1", "message": {"role": "assistant", "provider": "openrouter", "model": "x/y",
                "content": [{"type": "toolCall", "id": "c1", "name": "create_agent", "arguments": {"name": "worker", "task": "t"}}],
                "usage": {"input": 10, "output": 2, "cacheRead": 5, "cacheWrite": 0, "cost": {"total": 0.25}}, "stopReason": "toolUse", "durationMs": 7}}),
            ),
            line(
                json!({"type": "message", "id": "r1", "parentId": "a1", "message": {"role": "toolResult", "toolCallId": "c1", "content": [{"type": "text", "text": "created"}], "isError": false}}),
            ),
            line(
                json!({"type": "custom", "customType": MEMBER, "id": "mem", "parentId": "r1", "data": {
                "identity": {"name": "worker", "instanceId": "w1", "template": "subagent", "parentName": "main"}, "sessionFile": worker.display().to_string()}}),
            ),
            line(
                json!({"type": "custom_message", "customType": "noise", "id": "n1", "parentId": "mem", "content": "remove me"}),
            ),
            line(
                json!({"type": "context_edit", "id": "e1", "parentId": "n1", "targetId": "n1", "replacement": null}),
            ),
            line(
                json!({"type": "custom_message", "customType": MESSAGE, "id": "g1", "parentId": "e1", "content": "Message from worker",
                "details": {"id": "x", "from": "worker", "to": "main", "message": "done"}}),
            ),
            // A branch abandoned by tree navigation, written before the active leaf.
            line(
                json!({"type": "message", "id": "old", "parentId": "u1", "message": {"role": "user", "content": "abandoned"}}),
            ),
            line(
                json!({"type": "compaction", "id": "k1", "parentId": "g1", "summary": "sum", "tokensBefore": 99}),
            ),
        ];
        std::fs::write(&root, root_lines.join("\n")).unwrap();
        let worker_lines = [
            line(json!({"type": "session", "id": "wsess"})),
            line(
                json!({"type": "custom", "customType": IDENTITY, "id": "wi", "parentId": null, "data": {"name": "worker", "rootSessionFile": root_file}}),
            ),
            line(
                json!({"type": "message", "id": "wu", "parentId": "wi", "message": {"role": "user", "content": "task"}}),
            ),
        ];
        std::fs::write(&worker, worker_lines.join("\n")).unwrap();

        let trace = super::super::import_session(super::super::Harness::Pi, &worker)
            .await
            .unwrap();
        assert_eq!(trace.meta.id, "sess");
        assert_eq!(trace.agents.len(), 2);

        let main = &trace.agents[0];
        let types: Vec<_> = main.events.iter().map(|e| e.kind.type_name()).collect();
        assert_eq!(
            types,
            [
                "system",
                "user",
                "tool_call",
                "inference",
                "agent_message",
                "compaction"
            ]
        );
        assert!(
            matches!(&main.events[0].kind, EventKind::System { text } if text.starts_with("You are pi"))
        );
        let EventKind::Inference(inference) = &main.events[3].kind else {
            panic!()
        };
        assert_eq!(inference.input_tokens, Some(15));
        assert_eq!(inference.cost_usd, Some(0.25));
        assert_tool_output(&main.events[2], "created");
        assert_eq!(main.meta.model.as_deref(), Some("openrouter/x/y"));
        assert_eq!(main.meta.tools, ["bash", "create_agent"]);
        assert_eq!(
            trace.meta.tools["bash"].description.as_deref(),
            Some("Run commands")
        );

        let worker = &trace.agents[1];
        assert_eq!(worker.meta.id, "worker");
        assert_eq!(worker.meta.parent.as_deref(), Some("main"));
        assert_eq!(worker.meta.role.as_deref(), Some("subagent"));
        assert_eq!(worker.meta.spawned_by, Some(main.events[2].id));
    }
}
