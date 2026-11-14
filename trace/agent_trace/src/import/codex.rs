//! Codex: one `rollout-*.jsonl` per thread under `~/.codex/sessions/YYYY/MM/DD/`.
//! Subagent threads are separate files sharing the root's `session_id` and naming
//! their `parent_thread_id`. Logs are linear apart from `thread_rolled_back`.

use super::{
    ImportError, add_data_url_image, collect_jsonl_files, join_block_text, parse_arguments,
    parse_timestamp, read_first_record, read_jsonl, read_text, str_field, string_field, u64_field,
};
use crate::model::{
    AgentMeta, AgentTrace, Event, EventKind, FinishOutcome, ImageRef, Inference, SessionTrace,
    Source, ToolDefinition, ToolResult, attach_tool_result,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;

/// Identity fields from a thread's `session_meta` record.
#[derive(Debug, Clone)]
struct ThreadMeta {
    path: PathBuf,
    id: String,
    session_id: Option<String>,
    parent_id: Option<String>,
    payload: Value,
}

impl ThreadMeta {
    fn parse(path: &Path, record: &Value) -> Option<Self> {
        if str_field(record, "type") != Some("session_meta") {
            return None;
        }
        let payload = record.get("payload")?.clone();
        let spawn = payload.pointer("/source/subagent/thread_spawn");
        Some(Self {
            path: path.to_owned(),
            id: string_field(&payload, "id")?,
            session_id: string_field(&payload, "session_id"),
            parent_id: string_field(&payload, "parent_thread_id")
                .or_else(|| spawn.and_then(|spawn| string_field(spawn, "parent_thread_id"))),
            payload,
        })
    }

    fn root_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or(&self.id)
    }
}

pub(super) async fn import(path: &Path) -> Result<SessionTrace, ImportError> {
    let first = read_first_record(path).await?;
    let selected = first
        .as_ref()
        .and_then(|record| ThreadMeta::parse(path, record))
        .ok_or_else(|| ImportError::Invalid(format!("{}: missing session_meta", path.display())))?;
    let sessions_root = sessions_root(path);
    let threads = find_session_threads(&sessions_root, &selected).await?;
    let root = threads
        .iter()
        .find(|thread| thread.id == selected.root_id())
        .unwrap_or(&threads[0])
        .clone();

    let mut trace = SessionTrace::new(
        root.id.clone(),
        Source {
            harness: "codex".into(),
            version: string_field(&root.payload, "cli_version"),
            path: Some(root.path.display().to_string()),
        },
    );
    trace.meta.cwd = string_field(&root.payload, "cwd");
    trace.meta.started_at = parse_timestamp(root.payload.get("timestamp"));
    trace.meta.title = find_thread_name(&sessions_root, &root.id).await;

    for thread in &threads {
        let records = read_jsonl(&thread.path).await?;
        let agent = convert_thread(&mut trace, thread, thread.id == root.id, &records);
        trace.agents.push(agent);
    }
    Ok(trace)
}

/// The `sessions` directory containing `path`, or its parent directory.
fn sessions_root(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "sessions"))
        .or_else(|| path.parent())
        .unwrap_or(Path::new("."))
        .to_owned()
}

/// Every thread belonging to the selected thread's session, root first.
async fn find_session_threads(
    root: &Path,
    selected: &ThreadMeta,
) -> Result<Vec<ThreadMeta>, ImportError> {
    let mut all = HashMap::new();
    for path in collect_jsonl_files(root, 4).await? {
        let Ok(Some(record)) = read_first_record(&path).await else {
            continue;
        };
        if let Some(meta) = ThreadMeta::parse(&path, &record) {
            all.entry(meta.id.clone()).or_insert(meta);
        }
    }
    all.entry(selected.id.clone())
        .or_insert_with(|| selected.clone());

    // Older logs lack session_id; follow parent links to find the root instead.
    let root_of = |meta: &ThreadMeta| -> String {
        let mut current = meta;
        for _ in 0..64 {
            if let Some(session) = &current.session_id {
                return session.clone();
            }
            match current
                .parent_id
                .as_ref()
                .and_then(|parent| all.get(parent))
            {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
            .parent_id
            .clone()
            .unwrap_or_else(|| current.id.clone())
    };
    let session = root_of(selected);

    let mut threads: Vec<ThreadMeta> = all
        .values()
        .filter(|meta| meta.id == session || root_of(meta) == session)
        .cloned()
        .collect();
    threads.sort_by(|a, b| {
        (a.id != session)
            .cmp(&(b.id != session))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(threads)
}

/// Latest `thread_name` for the thread from `session_index.jsonl` beside `sessions/`.
async fn find_thread_name(sessions_root: &Path, id: &str) -> Option<String> {
    let index = sessions_root.parent()?.join("session_index.jsonl");
    let text = read_text(&index).await.ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| str_field(entry, "id") == Some(id))
        .find_map(|entry| string_field(&entry, "thread_name"))
}

struct Converter<'a> {
    trace: &'a mut SessionTrace,
    events: Vec<Event>,
    /// Source call id to tool call event id, for pairing results.
    calls: HashMap<String, u64>,
    tools: Vec<String>,
    model: Option<String>,
    effort: Option<String>,
    provider: Option<String>,
    base_instructions: Option<String>,
    /// Older logs lack per-response usage records and report cumulative token counts.
    use_token_counts: bool,
    last_total_tokens: Option<u64>,
}

fn convert_thread(
    trace: &mut SessionTrace,
    thread: &ThreadMeta,
    is_root: bool,
    records: &[Value],
) -> AgentTrace {
    let payload = &thread.payload;
    let spawn = payload.pointer("/source/subagent/thread_spawn");
    let mut meta = AgentMeta::new(thread.id.clone());
    meta.name = string_field(payload, "agent_path")
        .or_else(|| spawn.and_then(|spawn| string_field(spawn, "agent_path")))
        .or_else(|| string_field(payload, "agent_nickname"))
        .unwrap_or_else(|| {
            if is_root {
                "main".into()
            } else {
                thread.id.clone()
            }
        });
    meta.parent = (!is_root).then(|| thread.parent_id.clone()).flatten();
    meta.role = spawn
        .and_then(|spawn| string_field(spawn, "agent_role"))
        .or_else(|| (!is_root).then(|| "subagent".to_owned()));

    let base_instructions = payload
        .pointer("/base_instructions/text")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut converter = Converter {
        trace,
        events: Vec::new(),
        calls: HashMap::new(),
        tools: Vec::new(),
        model: None,
        effort: None,
        provider: string_field(payload, "model_provider"),
        base_instructions: base_instructions.clone(),
        use_token_counts: !records
            .iter()
            .any(|record| str_field(record, "type") == Some("token_usage_record")),
        last_total_tokens: None,
    };
    if let Some(text) = base_instructions {
        converter.push(
            parse_timestamp(payload.get("timestamp")),
            EventKind::System { text },
        );
    }
    for record in records {
        converter.apply_record(record);
    }

    meta.model = converter.events.iter().find_map(|event| match &event.kind {
        EventKind::Inference(inference) => inference.model.clone(),
        _ => None,
    });
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

    fn qualified_model(&self) -> Option<String> {
        let model = self.model.as_ref()?;
        Some(match &self.provider {
            Some(provider) => format!("{provider}/{model}"),
            None => model.clone(),
        })
    }

    fn apply_record(&mut self, record: &Value) {
        let t = parse_timestamp(record.get("timestamp"));
        let payload = record.get("payload").unwrap_or(&Value::Null);
        match str_field(record, "type").unwrap_or_default() {
            "turn_context" => self.apply_turn_context(t, payload),
            "response_item" => self.apply_response_item(t, payload),
            "compacted" => self.push(
                t,
                EventKind::Compaction {
                    summary: string_field(payload, "message").unwrap_or_default(),
                    tokens_before: None,
                },
            ),
            "token_usage_record" => {
                if let Some(usage) = payload.get("usage") {
                    self.push_usage(t, usage);
                }
            }
            "event_msg" => self.apply_event_msg(t, payload),
            _ => {}
        }
    }

    fn apply_turn_context(&mut self, t: Option<OffsetDateTime>, payload: &Value) {
        let model = string_field(payload, "model");
        let effort = string_field(payload, "effort").or_else(|| {
            payload
                .pointer("/collaboration_mode/settings/reasoning_effort")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let changed =
            (model.is_some() && model != self.model) || (effort.is_some() && effort != self.effort);
        let first = self.model.is_none();
        self.model = model.or(self.model.take());
        self.effort = effort.or(self.effort.take());
        if changed && !first {
            self.push(
                t,
                EventKind::ModelChange {
                    model: self.qualified_model(),
                    effort: self.effort.clone(),
                },
            );
        }
    }

    fn apply_response_item(&mut self, t: Option<OffsetDateTime>, item: &Value) {
        match str_field(item, "type").unwrap_or_default() {
            "message" => self.apply_message(t, item),
            "reasoning" => {
                let summary = join_block_text(
                    item.get("summary")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
                let raw = join_block_text(
                    item.get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
                let text = if raw.trim().is_empty() { summary } else { raw };
                if !text.trim().is_empty() {
                    self.push(t, EventKind::Thinking { text });
                }
            }
            "function_call" => self.push_call(
                t,
                string_field(item, "call_id").unwrap_or_default(),
                qualified_tool_name(item),
                parse_arguments(str_field(item, "arguments").unwrap_or_default()),
            ),
            "custom_tool_call" => self.push_call(
                t,
                string_field(item, "call_id").unwrap_or_default(),
                qualified_tool_name(item),
                Value::String(string_field(item, "input").unwrap_or_default()),
            ),
            "local_shell_call" | "web_search_call" | "tool_search_call" => {
                let name = str_field(item, "type")
                    .unwrap_or_default()
                    .trim_end_matches("_call");
                self.push_call(
                    t,
                    string_field(item, "call_id")
                        .or_else(|| string_field(item, "id"))
                        .unwrap_or_default(),
                    name.to_owned(),
                    item.get("action").cloned().unwrap_or(Value::Null),
                );
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                self.apply_tool_output(t, item)
            }
            "agent_message" => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|part| match str_field(part, "type") {
                        Some("encrypted_content") => "[encrypted]",
                        _ => str_field(part, "text").unwrap_or_default(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                self.push(
                    t,
                    EventKind::AgentMessage {
                        from: string_field(item, "author").unwrap_or_default(),
                        to: string_field(item, "recipient").unwrap_or_default(),
                        text,
                    },
                );
            }
            "additional_tools" => {
                for tool in item
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.add_tool_definition(tool, None);
                }
            }
            _ => {}
        }
    }

    fn apply_message(&mut self, t: Option<OffsetDateTime>, item: &Value) {
        let parts = item.get("content").and_then(Value::as_array);
        let text = join_block_text(parts.into_iter().flatten());
        let images: Vec<ImageRef> = parts
            .into_iter()
            .flatten()
            .filter(|part| str_field(part, "type") == Some("input_image"))
            .filter_map(|part| add_data_url_image(self.trace, str_field(part, "image_url")?))
            .collect();

        let kind = match str_field(item, "role").unwrap_or_default() {
            "assistant" => EventKind::Assistant { text },
            "system" => EventKind::System { text },
            "developer" => {
                if self.base_instructions.as_deref() == Some(text.as_str()) {
                    return;
                }
                EventKind::Context {
                    kind: "developer".into(),
                    text,
                }
            }
            _ if is_injected_context(&text) => EventKind::Context {
                kind: "instructions".into(),
                text,
            },
            _ => EventKind::User { text, images },
        };
        let empty = match &kind {
            EventKind::User { text, images } => text.trim().is_empty() && images.is_empty(),
            EventKind::Assistant { text }
            | EventKind::System { text }
            | EventKind::Context { text, .. } => text.trim().is_empty(),
            _ => false,
        };
        if !empty {
            self.push(t, kind);
        }
    }

    fn apply_tool_output(&mut self, t: Option<OffsetDateTime>, item: &Value) {
        let mut images = Vec::new();
        let mut duration_ms = None;
        let output = match item.get("output") {
            Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
                // Older shell outputs wrap the text with exit status metadata.
                Ok(wrapped) if wrapped.get("output").is_some_and(Value::is_string) => {
                    duration_ms = wrapped
                        .pointer("/metadata/duration_seconds")
                        .and_then(Value::as_f64)
                        .map(|seconds| (seconds * 1000.0) as u64);
                    string_field(&wrapped, "output").unwrap_or_default()
                }
                _ => text.clone(),
            },
            Some(Value::Array(parts)) => {
                for part in parts {
                    if str_field(part, "type") == Some("input_image")
                        && let Some(image) = str_field(part, "image_url")
                            .and_then(|url| add_data_url_image(self.trace, url))
                    {
                        images.push(image);
                    }
                }
                join_block_text(parts)
            }
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let call_id = str_field(item, "call_id").unwrap_or_default();
        let result = ToolResult {
            t,
            output,
            images,
            is_error: false,
            duration_ms,
        };
        self.attach_result(call_id, result);
    }

    fn apply_event_msg(&mut self, t: Option<OffsetDateTime>, payload: &Value) {
        match str_field(payload, "type").unwrap_or_default() {
            "token_count" if self.use_token_counts => {
                let Some(info) = payload.get("info") else {
                    return;
                };
                // Cumulative totals repeat when nothing new was billed.
                let total = info
                    .get("total_token_usage")
                    .and_then(|usage| u64_field(usage, "total_tokens"));
                if total.is_some() && total == self.last_total_tokens {
                    return;
                }
                self.last_total_tokens = total;
                if let Some(usage) = info.get("last_token_usage") {
                    self.push_usage(t, usage);
                }
            }
            "task_complete" => self.push(
                t,
                EventKind::AgentFinish {
                    outcome: FinishOutcome::Completed,
                    reason: None,
                    duration_ms: u64_field(payload, "duration_ms"),
                },
            ),
            "turn_aborted" => self.push(
                t,
                EventKind::AgentFinish {
                    outcome: FinishOutcome::Aborted,
                    reason: string_field(payload, "reason"),
                    duration_ms: None,
                },
            ),
            "error" => self.push(
                t,
                EventKind::Context {
                    kind: "error".into(),
                    text: string_field(payload, "message").unwrap_or_default(),
                },
            ),
            "thread_rolled_back" => {
                let turns = u64_field(payload, "num_turns").unwrap_or(1);
                for _ in 0..turns {
                    let last_user = self
                        .events
                        .iter()
                        .rposition(|event| matches!(event.kind, EventKind::User { .. }));
                    let Some(index) = last_user else {
                        break;
                    };
                    self.events.truncate(index);
                }
            }
            _ => {}
        }
    }

    fn push_usage(&mut self, t: Option<OffsetDateTime>, usage: &Value) {
        let cache_write = u64_field(usage, "cache_write_input_tokens").filter(|n| *n > 0);
        self.push(
            t,
            EventKind::Inference(Inference {
                model: self.qualified_model(),
                input_tokens: u64_field(usage, "input_tokens"),
                cached_input_tokens: u64_field(usage, "cached_input_tokens"),
                cache_write_tokens: cache_write,
                output_tokens: u64_field(usage, "output_tokens"),
                reasoning_tokens: u64_field(usage, "reasoning_output_tokens"),
                ..Inference::default()
            }),
        );
    }

    /// Definitions nest inside namespaces; the default `functions` namespace is unprefixed.
    fn add_tool_definition(&mut self, tool: &Value, namespace: Option<&str>) {
        let Some(name) = str_field(tool, "name") else {
            return;
        };
        if str_field(tool, "type") == Some("namespace") {
            let nested = (name != "functions").then_some(name);
            for inner in tool
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.add_tool_definition(inner, nested);
            }
            return;
        }

        let name = match namespace {
            Some(namespace) => format!("{namespace}.{name}"),
            None => name.to_owned(),
        };
        self.trace
            .meta
            .tools
            .entry(name.clone())
            .or_insert_with(|| ToolDefinition {
                description: string_field(tool, "description").filter(|d| !d.is_empty()),
                input_schema: tool.get("parameters").cloned(),
                input_format: tool.get("format").cloned(),
            });
        if !self.tools.contains(&name) {
            self.tools.push(name);
        }
    }
}

fn qualified_tool_name(item: &Value) -> String {
    let name = str_field(item, "name").unwrap_or_default();
    match str_field(item, "namespace") {
        Some(namespace) if namespace != "functions" => format!("{namespace}.{name}"),
        _ => name.to_owned(),
    }
}

/// Codex injects instructions and environment details as user-role messages.
fn is_injected_context(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<environment_context>")
        || text.starts_with("<user_instructions>")
        || text.starts_with("<permissions instructions>")
        || text.starts_with("<turn_aborted>")
        || text.starts_with("<subagent_notification>")
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
    async fn imports_threads_usage_and_rollbacks() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/11/08");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            home.path().join("session_index.jsonl"),
            line(json!({"id": "root", "thread_name": "Named"})),
        )
        .unwrap();

        let root = [
            line(
                json!({"timestamp": "2026-11-08T14:00:00Z", "type": "session_meta", "payload": {"id": "root", "session_id": "root",
                "cwd": "/w", "cli_version": "0.1", "model_provider": "openai", "base_instructions": {"text": "You are Codex"}}}),
            ),
            line(json!({"type": "turn_context", "payload": {"model": "gpt-x", "effort": "high"}})),
            line(
                json!({"type": "response_item", "payload": {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex"}]}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "first"}]}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "dropped"}]}}),
            ),
            line(
                json!({"type": "event_msg", "payload": {"type": "thread_rolled_back", "num_turns": 1}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second"}]}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "custom_tool_call", "call_id": "c1", "name": "exec", "input": "code()"}}),
            ),
            line(
                json!({"type": "token_usage_record", "payload": {"usage": {"input_tokens": 100, "cached_input_tokens": 80, "output_tokens": 5, "reasoning_output_tokens": 2}}}),
            ),
            line(
                json!({"type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 1}}}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "c1", "output": [{"type": "input_text", "text": "ok"}]}}),
            ),
            line(
                json!({"type": "event_msg", "payload": {"type": "task_complete", "duration_ms": 50}}),
            ),
        ];
        std::fs::write(day.join("rollout-a-root.jsonl"), root.join("\n")).unwrap();
        let child = [
            line(
                json!({"type": "session_meta", "payload": {"id": "child", "session_id": "root", "agent_path": "/root/worker",
                "source": {"subagent": {"thread_spawn": {"parent_thread_id": "root", "agent_role": "reviewer"}}}}}),
            ),
            line(
                json!({"type": "response_item", "payload": {"type": "agent_message", "author": "/root", "recipient": "/root/worker",
                "content": [{"type": "input_text", "text": "task: "}, {"type": "encrypted_content", "encrypted_content": "zzz"}]}}),
            ),
        ];
        std::fs::write(day.join("rollout-b-child.jsonl"), child.join("\n")).unwrap();
        std::fs::write(
            day.join("rollout-c-other.jsonl"),
            line(json!({"type": "session_meta", "payload": {"id": "other"}})),
        )
        .unwrap();

        let trace = super::super::import_session(
            super::super::Harness::Codex,
            &day.join("rollout-b-child.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(trace.meta.id, "root");
        assert_eq!(trace.meta.title.as_deref(), Some("Named"));
        assert_eq!(trace.agents.len(), 2);

        let main = &trace.agents[0];
        let types: Vec<_> = main.events.iter().map(|e| e.kind.type_name()).collect();
        assert_eq!(
            types,
            [
                "system",
                "context",
                "user",
                "tool_call",
                "inference",
                "agent_finish"
            ]
        );
        assert_eq!(main.meta.model.as_deref(), Some("openai/gpt-x"));
        assert!(
            matches!(&main.events[3].kind, EventKind::ToolCall { args: Value::String(code), .. } if code == "code()")
        );
        assert_tool_output(&main.events[3], "ok");

        let child = &trace.agents[1];
        assert_eq!(child.meta.name, "/root/worker");
        assert_eq!(child.meta.parent.as_deref(), Some("root"));
        assert_eq!(child.meta.role.as_deref(), Some("reviewer"));
        assert!(
            matches!(&child.events[0].kind, EventKind::AgentMessage { text, .. } if text == "task: [encrypted]")
        );
    }
}
