//! In-memory trace types and their on-disk JSON shapes.
//!
//! A trace is intentionally lossy: it keeps the active conversation branch of each
//! agent as a linear event list, enough for analysis rather than faithful replay.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use time::OffsetDateTime;

/// Version written to `session.json`; readers reject any other version.
pub const FORMAT_VERSION: u32 = 4;

/// A whole session: metadata, one linear event log per agent, and binary blobs.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTrace {
    pub meta: SessionMeta,
    pub agents: Vec<AgentTrace>,
    /// Blob file name (relative to `blobs/`) to contents.
    pub blobs: BTreeMap<String, Vec<u8>>,
    next_event_id: u64,
}

/// One agent's metadata and its active-branch events in order.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTrace {
    pub meta: AgentMeta,
    pub events: Vec<Event>,
}

/// Contents of `session.json`, excluding agent events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub format_version: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub ended_at: Option<OffsetDateTime>,
    /// Tool definitions shared by all agents, keyed by model-facing name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolDefinition>,
    /// Agents in display order; the first is normally the root.
    pub agents: Vec<AgentMeta>,
}

/// Where a trace came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Harness name such as `claude-code`, `codex`, `pi`, or `psa`.
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Original location, such as the imported session file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Source {
    pub fn default_for(harness: &str) -> Self {
        Self {
            harness: harness.to_owned(),
            version: None,
            path: None,
        }
    }
}

/// A model-facing tool description. Fields are absent when the source did not record them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON schema of the arguments, for function-style tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Non-JSON input format, such as a grammar for freeform tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_format: Option<Value>,
}

/// Agent metadata stored in `session.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMeta {
    /// Unique within the session; used in references between agents.
    pub id: String,
    /// Human-facing name, often equal to the id.
    pub name: String,
    /// Event log path relative to the session directory; assigned when saving.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Event id of the tool call that created this agent, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<u64>,
    /// Template, agent type, or role, in the source harness's terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Initial model, as `provider/model` when the provider is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Names of tools available to this agent; definitions live in `SessionMeta::tools`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl AgentMeta {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            file: String::new(),
            parent: None,
            spawned_by: None,
            role: None,
            model: None,
            tools: Vec::new(),
        }
    }
}

/// One line of an agent log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Sequential and unique across the whole session; see [`SessionTrace::update_event_ids`].
    pub id: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub t: Option<OffsetDateTime>,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    /// An event with id 0; the trace assigns real ids once all events are collected.
    pub fn new(t: Option<OffsetDateTime>, kind: EventKind) -> Self {
        Self { id: 0, t, kind }
    }
}

/// Event payloads. Only `type` and [`FinishOutcome`] are closed sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// System prompt text, with any sections joined.
    System {
        text: String,
    },
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    Assistant {
        text: String,
    },
    /// Readable reasoning; empty reasoning (encrypted or redacted) is omitted by importers.
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        /// Parsed JSON arguments, or a string when they were not JSON.
        args: Value,
        /// `None` when the session stopped before a result was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ToolResult>,
    },
    /// Context injected by the harness that the model saw, such as reminders or hooks.
    Context {
        kind: String,
        text: String,
    },
    /// A message received from another agent.
    AgentMessage {
        from: String,
        to: String,
        text: String,
    },
    Compaction {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
    },
    /// Accounting for one model response.
    Inference(Inference),
    ModelChange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    AgentFinish {
        outcome: FinishOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

impl EventKind {
    /// The serialized `type` tag.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::Thinking { .. } => "thinking",
            Self::ToolCall { .. } => "tool_call",
            Self::Context { .. } => "context",
            Self::AgentMessage { .. } => "agent_message",
            Self::Compaction { .. } => "compaction",
            Self::Inference(_) => "inference",
            Self::ModelChange { .. } => "model_change",
            Self::AgentFinish { .. } => "agent_finish",
        }
    }

    pub const TYPE_NAMES: [&'static str; 11] = [
        "system",
        "user",
        "assistant",
        "thinking",
        "tool_call",
        "context",
        "agent_message",
        "compaction",
        "inference",
        "model_change",
        "agent_finish",
    ];
}

/// The result of a [`EventKind::ToolCall`], stored on the call itself.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolResult {
    /// When the result was recorded.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub t: Option<OffsetDateTime>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Store `result` on the unanswered tool call with event id `call`. Returns false when
/// there is no such call, such as when it was dropped or already answered; callers
/// discard the result.
pub fn attach_tool_result(events: &mut [Event], call: u64, result: ToolResult) -> bool {
    let event = events.iter_mut().find(|event| event.id == call);
    let Some(Event {
        kind: EventKind::ToolCall {
            result: slot @ None,
            ..
        },
        ..
    }) = event
    else {
        return false;
    };
    *slot = Some(result);
    true
}

/// Token counts follow OpenAI semantics: `input_tokens` includes cached reads and
/// cache writes, and `reasoning_tokens` is a subset of `output_tokens`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Inference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishOutcome {
    Completed,
    Aborted,
    Error,
}

impl FinishOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Error => "error",
        }
    }
}

/// An image stored in the session's `blobs/` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub mime: String,
    /// File name inside `blobs/`.
    pub blob: String,
}

impl SessionTrace {
    pub fn new(id: impl Into<String>, source: Source) -> Self {
        Self {
            meta: SessionMeta {
                format_version: FORMAT_VERSION,
                id: id.into(),
                title: None,
                source,
                cwd: None,
                started_at: None,
                ended_at: None,
                tools: BTreeMap::new(),
                agents: Vec::new(),
            },
            agents: Vec::new(),
            blobs: BTreeMap::new(),
            next_event_id: 0,
        }
    }

    /// A loaded trace without blobs; new event ids continue after the largest one.
    pub(crate) fn from_parts(meta: SessionMeta, agents: Vec<AgentTrace>) -> Self {
        let events = agents.iter().flat_map(|agent| &agent.events);
        let next_event_id = events.map(|event| event.id + 1).max().unwrap_or(0);
        Self {
            meta,
            agents,
            blobs: BTreeMap::new(),
            next_event_id,
        }
    }

    /// A fresh event id, unique among events created through this trace. Ids become
    /// sequential when [`Self::update_event_ids`] runs.
    pub fn allocate_event_id(&mut self) -> u64 {
        let id = self.next_event_id;
        self.next_event_id += 1;
        id
    }

    /// Store bytes content-addressed and return a reference to them.
    pub fn add_image(&mut self, mime: &str, bytes: Vec<u8>) -> ImageRef {
        let digest = Sha256::digest(&bytes);
        let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        let blob = format!("{hex}.{}", extension_for_mime(mime));
        self.blobs.entry(blob.clone()).or_insert(bytes);
        ImageRef {
            mime: mime.to_owned(),
            blob,
        }
    }

    pub fn agent(&self, id: &str) -> Option<&AgentTrace> {
        self.agents.iter().find(|agent| agent.meta.id == id)
    }

    /// The root agent: the first one without a parent, else the first agent.
    pub fn root_agent(&self) -> Option<&AgentTrace> {
        self.agents
            .iter()
            .find(|agent| agent.meta.parent.is_none())
            .or_else(|| self.agents.first())
    }

    /// Number every event sequentially from 0, agent by agent in display order, and
    /// rewrite `spawned_by` references to match. A reference to a missing event is dropped.
    pub fn update_event_ids(&mut self) {
        let mut renumbered = HashMap::new();
        let mut next_id = 0;
        for event in self.agents.iter_mut().flat_map(|agent| &mut agent.events) {
            renumbered.entry(event.id).or_insert(next_id);
            event.id = next_id;
            next_id += 1;
        }
        self.next_event_id = next_id;

        for agent in &mut self.agents {
            let spawned_by = agent.meta.spawned_by.and_then(|old| renumbered.get(&old));
            agent.meta.spawned_by = spawned_by.copied();
        }
    }

    /// Find an event by its session-wide id, with the agent that owns it.
    pub fn find_event(&self, id: u64) -> Option<(&AgentTrace, &Event)> {
        self.agents.iter().find_map(|agent| {
            let event = agent.events.iter().find(|event| event.id == id)?;
            Some((agent, event))
        })
    }

    /// Fill `started_at`/`ended_at` from event timestamps when the source omitted them.
    pub fn update_time_bounds(&mut self) {
        let times = self.agents.iter().flat_map(|agent| {
            agent.events.iter().flat_map(|event| {
                let result_time = match &event.kind {
                    EventKind::ToolCall { result, .. } => result.as_ref().and_then(|r| r.t),
                    _ => None,
                };
                event.t.into_iter().chain(result_time)
            })
        });
        let (mut first, mut last) = (None::<OffsetDateTime>, None::<OffsetDateTime>);
        for t in times {
            first = Some(first.map_or(t, |current| current.min(t)));
            last = Some(last.map_or(t, |current| current.max(t)));
        }
        self.meta.started_at = self.meta.started_at.or(first);
        self.meta.ended_at = self.meta.ended_at.or(last);
    }

    /// Ensure every tool an agent called is listed on that agent and in the session.
    pub fn register_used_tools(&mut self) {
        for agent in &mut self.agents {
            for event in &agent.events {
                let EventKind::ToolCall { name, .. } = &event.kind else {
                    continue;
                };
                if !agent.meta.tools.contains(name) {
                    agent.meta.tools.push(name.clone());
                }
                self.meta.tools.entry(name.clone()).or_default();
            }
        }
    }
}

fn extension_for_mime(mime: &str) -> &str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_serialize_flat_with_type_tag() {
        let mut event = Event::new(
            None,
            EventKind::ToolCall {
                name: "bash".into(),
                args: json!({}),
                result: Some(ToolResult {
                    output: "ok".into(),
                    duration_ms: Some(5),
                    ..Default::default()
                }),
            },
        );
        event.id = 7;
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            json!({
                "id": 7,
                "type": "tool_call",
                "name": "bash",
                "args": {},
                "result": {"output": "ok", "duration_ms": 5},
            })
        );
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);

        let inference = Event::new(
            None,
            EventKind::Inference(Inference {
                input_tokens: Some(10),
                ..Default::default()
            }),
        );
        let value = serde_json::to_value(&inference).unwrap();
        assert_eq!(
            value,
            json!({"id": 0, "type": "inference", "input_tokens": 10})
        );
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), inference);
    }

    #[test]
    fn results_attach_once_to_unanswered_calls() {
        let call = |id: u64| Event {
            id,
            ..Event::new(
                None,
                EventKind::ToolCall {
                    name: "bash".into(),
                    args: json!({}),
                    result: None,
                },
            )
        };
        let output = |text: &str| ToolResult {
            output: text.into(),
            ..Default::default()
        };
        let mut events = vec![
            call(4),
            call(5),
            call(6),
            Event::new(None, EventKind::Assistant { text: "hi".into() }),
        ];

        assert!(attach_tool_result(&mut events, 6, output("second")));
        assert!(attach_tool_result(&mut events, 4, output("first")));
        assert!(!attach_tool_result(&mut events, 4, output("again")));
        assert!(!attach_tool_result(&mut events, 0, output("not a call")));
        assert!(!attach_tool_result(&mut events, 9, output("missing")));

        let outputs: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolCall { result, .. } => {
                    Some(result.as_ref().map(|r| r.output.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(outputs, [Some("first"), None, Some("second")]);
    }

    #[test]
    fn event_ids_are_sequential_across_agents() {
        let mut trace = SessionTrace::new("s", Source::default_for("test"));
        trace.allocate_event_id(); // An event that was dropped during import.
        for name in ["main", "sub"] {
            let mut events = Vec::new();
            for _ in 0..2 {
                let id = trace.allocate_event_id();
                let text = name.into();
                events.push(Event {
                    id,
                    ..Event::new(None, EventKind::Assistant { text })
                });
            }
            trace.agents.push(AgentTrace {
                meta: AgentMeta::new(name),
                events,
            });
        }
        trace.agents[1].meta.spawned_by = Some(2);

        trace.update_event_ids();

        let ids: Vec<u64> = trace
            .agents
            .iter()
            .flat_map(|agent| agent.events.iter().map(|event| event.id))
            .collect();
        assert_eq!(ids, [0, 1, 2, 3]);
        assert_eq!(trace.agents[1].meta.spawned_by, Some(1));
        assert_eq!(trace.allocate_event_id(), 4);
        let (agent, _) = trace.find_event(2).unwrap();
        assert_eq!(agent.meta.id, "sub");
    }

    #[test]
    fn images_are_content_addressed() {
        let mut trace = SessionTrace::new("s", Source::default_for("test"));
        let first = trace.add_image("image/png", vec![1, 2, 3]);
        let second = trace.add_image("image/png", vec![1, 2, 3]);
        assert_eq!(first, second);
        assert!(first.blob.ends_with(".png"));
        assert_eq!(trace.blobs.len(), 1);
    }
}
