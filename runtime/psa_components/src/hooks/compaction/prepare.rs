//! Pure context compaction: safe cuts, standalone prompts, and cumulative file tracking.
//! Adapted from earendil-works/pi (MIT); see docs/PI_LICENSE.txt.

use super::prompts;
use llm_api::{
    api::{ContentBlock, Outcome, Usage},
    providers::ModelConfig,
};
use psa::api::events::{CompactionSummary, Event, EventType, MessageType};
use psa_store::NewEvent;
use std::collections::{BTreeSet, HashSet};

#[derive(Debug)]
pub struct Preparation {
    pub previous_summary: Option<String>,
    pub history_prompt: Option<String>,
    pub turn_prefix_prompt: Option<String>,
    /// Conversation replay only: no control events, prompt snapshots, or inference delimiters.
    pub retained_events: Vec<NewEvent>,
    /// Model active at the cut; later model changes remain in `retained_events`.
    pub retained_model: Option<ModelConfig>,
    pub tokens_before: u64,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl Preparation {
    /// Combine the requested completions exactly as Pi does, then append file tracking.
    pub fn finish_summary(&self, history: Option<&str>, turn_prefix: Option<&str>) -> String {
        let history = history
            .or(self.previous_summary.as_deref())
            .unwrap_or("No prior history.");
        let mut summary = if let Some(prefix) = turn_prefix {
            format!("{history}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}")
        } else {
            history.to_owned()
        };
        for (tag, files) in [
            ("read-files", &self.read_files),
            ("modified-files", &self.modified_files),
        ] {
            if !files.is_empty() {
                summary.push_str(&format!("\n\n<{tag}>\n{}\n</{tag}>", files.join("\n")));
            }
        }
        summary
    }
}

/// Prepare only when at least one conversation entry can be discarded safely.
/// Inference responses and all their tool-call/result pairs are indivisible at cuts.
/// Native compaction is deliberately not converted into a lossy text checkpoint.
pub fn prepare(events: &[Event], keep_recent_tokens: u64) -> Option<Preparation> {
    let events = current_context(events);
    let previous = events
        .iter()
        .rposition(|event| matches!(event.kind, EventType::Compaction(_)));
    let previous_summary = match previous.map(|index| &events[index].kind) {
        Some(EventType::Compaction(compaction)) => match &compaction.summary {
            CompactionSummary::Text(summary) => Some(summary.clone()),
            CompactionSummary::ProviderItem(_) => return None,
        },
        _ => None,
    };
    let start = previous.map_or(0, |index| index + 1);
    let candidates = find_cut_points(&events[start..])?;
    let first = candidates.first()?.0 + start;
    let estimates = token_estimates(events);
    let mut cut = first;
    let mut accumulated = 0_u64;
    for index in (start..events.len()).rev() {
        if estimates[index] == 0 {
            continue;
        }
        accumulated = accumulated.saturating_add(estimates[index]);
        if accumulated >= keep_recent_tokens {
            cut = candidates
                .iter()
                .find(|(candidate, _)| candidate + start >= index)
                .or_else(|| candidates.last())?
                .0
                + start;
            break;
        }
    }

    let starts_turn = candidates
        .iter()
        .any(|&(index, turn)| index + start == cut && turn);
    let turn_start = if starts_turn {
        None
    } else {
        candidates
            .iter()
            .rev()
            .find(|&&(index, turn)| index + start < cut && turn)
            .map(|&(index, _)| index + start)
    };
    let history_end = turn_start.unwrap_or(cut);
    let history_events = &events[start..history_end];
    let prefix_events = turn_start.map_or(&[][..], |index| &events[index..cut]);
    let has_history = history_events
        .iter()
        .any(|event| is_conversation(&event.kind));
    let has_prefix = prefix_events
        .iter()
        .any(|event| is_conversation(&event.kind));
    if !has_history && !has_prefix {
        return None;
    }
    let history = serialize_conversation(history_events);
    let prefix = serialize_conversation(prefix_events);

    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    if let Some(summary) = &previous_summary {
        read.extend(parse_file_list(summary, "read-files"));
        modified.extend(parse_file_list(summary, "modified-files"));
    }
    for event in &events[start..cut] {
        if let EventType::ToolCall(call) = &event.kind {
            let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
                continue;
            };
            let Some(path) = arguments.get("path").and_then(|path| path.as_str()) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            match call.tool_name.as_str() {
                "read" => {
                    read.insert(path.to_owned());
                }
                "write" | "edit" => {
                    modified.insert(path.to_owned());
                }
                _ => {}
            }
        }
    }
    read.retain(|path| !modified.contains(path));

    let retained_model = events[..cut]
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            EventType::Model(model) => Some(model.clone()),
            _ => None,
        });
    let retained_events = events[cut..]
        .iter()
        .filter(|event| is_retained(&event.kind))
        .map(|event| NewEvent {
            timestamp: event.timestamp,
            kind: event.kind.clone(),
        })
        .collect();
    Some(Preparation {
        history_prompt: has_history
            .then(|| prompts::history_prompt(&history, previous_summary.as_deref())),
        turn_prefix_prompt: has_prefix.then(|| prompts::turn_prefix_prompt(&prefix)),
        previous_summary,
        retained_events,
        retained_model,
        tokens_before: estimate_context_tokens(events),
        read_files: read.into_iter().collect(),
        modified_files: modified.into_iter().collect(),
    })
}

fn current_context(events: &[Event]) -> &[Event] {
    let start = events
        .iter()
        .rposition(|event| matches!(event.kind, EventType::NewContext));
    &events[start.map_or(0, |index| index + 1)..]
}

fn is_retained(kind: &EventType) -> bool {
    is_conversation(kind) || matches!(kind, EventType::Model(_))
}

fn is_conversation(kind: &EventType) -> bool {
    matches!(kind, EventType::Message(message) if message.message_type != MessageType::System)
        || is_assistant_event(kind)
        || matches!(kind, EventType::ToolResult(_))
}

/// Candidate indices are entry starts, never tool results or the middle of a response.
fn find_cut_points(events: &[Event]) -> Option<Vec<(usize, bool)>> {
    let mut candidates = Vec::new();
    let mut open = None;
    let mut pending = HashSet::new();
    let mut assistant_run = false;
    for (index, event) in events.iter().enumerate() {
        if open.is_none() && pending.is_empty() {
            let turn = match &event.kind {
                kind if is_assistant_event(kind) && !assistant_run => Some(false),
                EventType::Message(message)
                    if message.message_type != MessageType::System
                        && message.message_type != MessageType::Agent =>
                {
                    Some(true)
                }
                EventType::InferenceResultStart(_) => Some(false),
                _ => None,
            };
            if let Some(turn) = turn {
                candidates.push((index, turn));
            }
        }
        match &event.kind {
            EventType::InferenceResultStart(start) => {
                if open.replace(start.id).is_some() {
                    return None;
                }
            }
            EventType::InferenceResultEnd(end) => {
                if open.take() != Some(end.id) {
                    return None;
                }
            }
            EventType::ToolCall(call) => {
                if !pending.insert(call.call_id.as_str()) {
                    return None;
                }
            }
            EventType::ToolResult(result) => {
                if open.is_some() || !pending.remove(result.call_id.as_str()) {
                    return None;
                }
            }
            _ => {}
        }
        if open.is_none() {
            match &event.kind {
                kind if is_assistant_event(kind) => assistant_run = true,
                EventType::Message(_)
                | EventType::ToolResult(_)
                | EventType::Model(_)
                | EventType::ToolDefinition(_)
                | EventType::InferenceResultEnd(_) => assistant_run = false,
                _ => {}
            }
        }
    }
    (open.is_none() && pending.is_empty()).then_some(candidates)
}

fn is_assistant_event(kind: &EventType) -> bool {
    matches!(kind, EventType::Message(message) if message.message_type == MessageType::Agent)
        || matches!(
            kind,
            EventType::Thinking(_) | EventType::ToolCall(_) | EventType::ProviderItem(_)
        )
}

/// Latest usable response accounting plus estimates for later context. Cache/reasoning
/// counts in PSA are subsets, not extra tokens. Compaction invalidates earlier usage.
pub fn estimate_context_tokens(events: &[Event]) -> u64 {
    let events = current_context(events);
    let mut latest = None;
    for (index, event) in events.iter().enumerate() {
        match &event.kind {
            EventType::Compaction(_) => latest = None,
            EventType::InferenceResultEnd(end)
                if matches!(
                    end.outcome,
                    Outcome::Completed { .. } | Outcome::Incomplete { .. }
                ) =>
            {
                let tokens = usage_tokens(&end.usage);
                if tokens > 0 {
                    latest = Some((index, tokens));
                }
            }
            _ => {}
        }
    }
    let estimates = token_estimates(events);
    let (start, tokens) = latest.map_or((0, 0), |(index, tokens)| (index + 1, tokens));
    estimates[start..]
        .iter()
        .fold(tokens, |sum, tokens| sum.saturating_add(*tokens))
}

fn usage_tokens(usage: &Usage) -> u64 {
    usage
        .total_tokens
        .filter(|tokens| *tokens > 0)
        .unwrap_or_else(|| {
            usage
                .input_tokens
                .unwrap_or(0)
                .saturating_add(usage.output_tokens.unwrap_or(0))
        })
}

// JS string.length counts UTF-16 units. Match Pi rather than counting UTF-8 bytes.
fn char_count(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn content_chars(content: &[ContentBlock]) -> u64 {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => char_count(&text.text),
            ContentBlock::Image(_) => 4800,
            _ => 0,
        })
        .sum()
}

fn event_chars(kind: &EventType) -> u64 {
    match kind {
        EventType::Message(message) => {
            content_chars(&message.content) + char_count(&subagent_prefix(message))
        }
        EventType::ToolResult(result) => content_chars(&result.content),
        EventType::Thinking(thinking) => thinking
            .contents
            .summary
            .iter()
            .map(|text| char_count(text))
            .sum(),
        EventType::ToolCall(call) => {
            let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| call.arguments.clone());
            char_count(&call.tool_name) + char_count(&arguments)
        }
        EventType::Compaction(compaction) => match &compaction.summary {
            CompactionSummary::Text(text) => char_count(text),
            CompactionSummary::ProviderItem(_) => 0,
        },
        EventType::ToolDefinition(tool) => {
            serde_json::to_string(&tool.definition).map_or(0, |text| char_count(&text))
        }
        _ => 0,
    }
}

/// Round once for each response, including assistant runs replayed without delimiters.
fn token_estimates(events: &[Event]) -> Vec<u64> {
    let mut tokens = vec![0; events.len()];
    let mut assistant = None;
    let mut in_result = false;
    for (index, event) in events.iter().enumerate() {
        let starts = matches!(event.kind, EventType::InferenceResultStart(_));
        let ends = matches!(event.kind, EventType::InferenceResultEnd(_));
        let output = is_assistant_event(&event.kind);
        let boundary = matches!(
            event.kind,
            EventType::Message(_)
                | EventType::ToolResult(_)
                | EventType::Model(_)
                | EventType::ToolDefinition(_)
                | EventType::Compaction(_)
                | EventType::NewContext
        );
        if starts || ends || (!in_result && !output && boundary) {
            if let Some((start, chars)) = assistant.take() {
                tokens[start] = u64::div_ceil(chars, 4);
            }
        }
        if starts {
            assistant = Some((index, 0_u64));
            in_result = true;
        } else if ends {
            in_result = false;
        } else if in_result || output {
            let (_, chars) = assistant.get_or_insert((index, 0));
            *chars = chars.saturating_add(event_chars(&event.kind));
        } else {
            tokens[index] = event_chars(&event.kind).div_ceil(4);
        }
    }
    if let Some((start, chars)) = assistant {
        tokens[start] = chars.div_ceil(4);
    }
    tokens
}

fn text_content(content: &[ContentBlock], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn subagent_prefix(message: &psa::api::events::Message) -> String {
    match &message.source {
        Some(source) => crate::prompts::agent_message_header(&source.sender_name),
        None => String::new(),
    }
}

fn serialize_conversation(events: &[Event]) -> String {
    let mut parts = Vec::new();
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for event in events {
        match &event.kind {
            EventType::InferenceResultStart(_) => {
                flush_assistant(&mut parts, &mut text, &mut calls)
            }
            EventType::InferenceResultEnd(_) => flush_assistant(&mut parts, &mut text, &mut calls),
            EventType::Message(message) if message.message_type == MessageType::Agent => {
                if message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text(_)))
                {
                    text.push(text_content(&message.content, "\n"));
                }
            }
            EventType::ToolCall(call) => {
                let args = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
                let args = args
                    .as_ref()
                    .and_then(|value| value.as_object())
                    .map(|args| {
                        args.iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| call.arguments.clone());
                calls.push(format!("{}({args})", call.tool_name));
            }
            EventType::Message(message) if message.message_type != MessageType::System => {
                flush_assistant(&mut parts, &mut text, &mut calls);
                let content = format!(
                    "{}{}",
                    subagent_prefix(message),
                    text_content(&message.content, "")
                );
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            EventType::ToolResult(result) => {
                flush_assistant(&mut parts, &mut text, &mut calls);
                let content = text_content(&result.content, "");
                if !content.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_result(&content)));
                }
            }
            // Thinking (including opaque state), systems, and provider-only state are not transcript text.
            _ => {}
        }
    }
    flush_assistant(&mut parts, &mut text, &mut calls);
    parts.join("\n\n")
}

fn flush_assistant(parts: &mut Vec<String>, text: &mut Vec<String>, calls: &mut Vec<String>) {
    if !text.is_empty() {
        parts.push(format!("[Assistant]: {}", text.join("\n")));
        text.clear();
    }
    if !calls.is_empty() {
        parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
        calls.clear();
    }
}

fn truncate_result(text: &str) -> String {
    let chars = char_count(text);
    if chars <= 2000 {
        return text.to_owned();
    }
    let mut kept = 0;
    let end = text
        .char_indices()
        .find_map(|(index, ch)| {
            kept += ch.len_utf16() as u64;
            (kept > 2000).then_some(index)
        })
        .unwrap_or(text.len());
    // Never split a Unicode scalar even if the JS limit falls inside a surrogate pair.
    let prefix = &text[..end];
    format!(
        "{prefix}\n\n[... {} more characters truncated]",
        chars - char_count(prefix)
    )
}

fn parse_file_list(summary: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>\n");
    let close = format!("\n</{tag}>");
    summary
        .split(&open)
        .skip(1)
        .filter_map(|section| section.split_once(&close))
        .flat_map(|(files, _)| {
            files
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
#[path = "../../tests/hooks/compaction/prepare/tests.rs"]
mod tests;
