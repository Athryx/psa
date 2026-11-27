//! Transcript rows as styled terminal lines.
use super::{
    commands::describe_count,
    settings::TuiPreferences,
    text::{truncate_line, wrap_text},
    theme::Palette,
};
use client_shared::{
    ReasoningDisplay, RowPayload, Transcript, TurnDividerDisplay, describe_command,
    is_successful_inference, summarize_decision_call,
};
use llm_api::api::{ContentBlock, Outcome};
use psa::api::{
    AgentResult, AgentState,
    events::{CompactionSummary, EventType, MessageType, ToolCall},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

/// Rows rendered from the end of the loaded transcript.
const RENDERED_ROWS: usize = 400;
/// Argument fields that best describe a tool call, in order of preference.
const SUMMARY_FIELDS: [&str; 8] = [
    "command",
    "cmd",
    "path",
    "file_path",
    "pattern",
    "query",
    "url",
    "code",
];

/// What to show and how.
pub struct DisplayOptions<'a> {
    pub preferences: &'a TuiPreferences,
    pub palette: &'a Palette,
    /// The Ctrl+O view: also system messages and full tool arguments.
    pub details: bool,
}

/// One transcript entry: a two-column marker, then styled paragraphs.
struct Item {
    marker: &'static str,
    marker_style: Style,
    paragraphs: Vec<(String, Style)>,
}

impl Item {
    fn new(marker: &'static str, marker_style: Style) -> Self {
        Self {
            marker,
            marker_style,
            paragraphs: Vec::new(),
        }
    }

    fn with(mut self, text: impl Into<String>, style: Style) -> Self {
        self.paragraphs.push((text.into(), style));
        self
    }
}

pub fn build_lines(
    transcript: &Transcript,
    options: &DisplayOptions,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width).max(12);
    let count = transcript.row_count();
    let start = count.saturating_sub(RENDERED_ROWS);
    let mut lines = Vec::new();
    if start > 0 || transcript.has_older {
        lines.push(Line::styled(
            "… earlier history not shown",
            options.palette.muted,
        ));
    }

    for payload in transcript.row_window(start, RENDERED_ROWS) {
        let Some(item) = describe_row(transcript, &payload, options, width) else {
            continue;
        };
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        append_item(&mut lines, item, width);
    }
    lines
}

fn append_item(lines: &mut Vec<Line<'static>>, item: Item, width: usize) {
    let mut first = true;
    for (text, style) in item.paragraphs {
        for line in wrap_text(&text, width - 2) {
            let marker = if first {
                Span::styled(item.marker, item.marker_style)
            } else {
                Span::raw("  ")
            };
            first = false;
            lines.push(Line::from(vec![marker, Span::styled(line, style)]));
        }
    }
}

fn describe_row(
    transcript: &Transcript,
    payload: &RowPayload,
    options: &DisplayOptions,
    width: usize,
) -> Option<Item> {
    let preferences = options.preferences;
    let palette = options.palette;
    let muted = palette.muted;
    match payload.kind() {
        EventType::Message(message) => {
            let text = content_text(&message.content);
            match message.message_type {
                MessageType::Human => Some(
                    Item::new("› ", palette.user.add_modifier(Modifier::BOLD))
                        .with(text, palette.user),
                ),
                MessageType::Agent => Some(Item::new("• ", Style::new()).with(text, Style::new())),
                MessageType::Subagent => {
                    let sender = message
                        .source
                        .as_ref()
                        .map(|source| source.sender_name.as_str())
                        .unwrap_or("another agent");
                    Some(
                        Item::new("↳ ", palette.subagent)
                            .with(
                                format!("from {sender}"),
                                palette.subagent.add_modifier(Modifier::BOLD),
                            )
                            .with(text, Style::new()),
                    )
                }
                MessageType::Harness if !preferences.show_harness_messages => None,
                MessageType::System if !options.details => None,
                MessageType::Harness | MessageType::System => {
                    let label =
                        message
                            .description
                            .as_deref()
                            .unwrap_or(match message.message_type {
                                MessageType::System => "system",
                                _ => "harness",
                            });
                    Some(Item::new("· ", muted).with(format!("{label}\n{text}"), muted))
                }
            }
        }
        EventType::Thinking(thinking) => {
            let summary = thinking.contents.summary.join("\n");
            if summary.trim().is_empty() {
                return None;
            }
            let style = muted.add_modifier(Modifier::ITALIC);
            let text = match preferences.reasoning_display {
                ReasoningDisplay::Hidden => return None,
                ReasoningDisplay::Collapsed => truncate_line(summary.trim(), width - 2),
                ReasoningDisplay::Shown => summary,
            };
            Some(Item::new("∴ ", style).with(text, style))
        }
        EventType::ToolCall(call) => Some(describe_tool_call(
            transcript, payload, call, options, width,
        )),
        EventType::ToolResult(_) => None,
        EventType::Result(AgentResult::Text(_)) if preferences.hide_text_recorded_results => None,
        EventType::Result(result) => {
            let text = match result {
                AgentResult::Text(text) => text.clone(),
                AgentResult::Json(value) => {
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
                }
            };
            Some(
                Item::new("✓ ", palette.success)
                    .with("Result", palette.success.add_modifier(Modifier::BOLD))
                    .with(text, Style::new()),
            )
        }
        EventType::Compaction(compaction) => {
            let mut item = Item::new("≡ ", muted).with("Context compacted", muted);
            if let (true, CompactionSummary::Text(summary)) = (options.details, &compaction.summary)
            {
                item = item.with(summary.clone(), muted);
            }
            Some(item)
        }
        EventType::Log(log) if !preferences.log_level.shows(log.error) => None,
        EventType::Log(log) if log.error => {
            Some(Item::new("✗ ", palette.error).with(log.message.clone(), palette.error))
        }
        EventType::Log(log) => Some(Item::new("· ", muted).with(log.message.clone(), muted)),
        EventType::Command(command) => {
            Some(Item::new("  ", muted).with(describe_command(command), muted))
        }
        EventType::InferenceResultStart(_) => describe_turn(transcript, payload, options, width),
        EventType::InferenceResultEnd(end) if !is_successful_inference(end) => {
            Some(Item::new("✗ ", palette.error).with(describe_outcome(&end.outcome), palette.error))
        }
        EventType::StatusChange(change) if change.new_status == AgentState::Interrupted => {
            let text = match &change.reason {
                Some(reason) => format!("Interrupted: {reason}"),
                None => "Interrupted".into(),
            };
            Some(Item::new("■ ", palette.warning).with(text, muted))
        }
        EventType::StatusChange(change) if !preferences.hide_agent_statuses => {
            let state = match change.new_status {
                AgentState::Running => "Running",
                AgentState::Idle => "Idle",
                AgentState::Interrupted => "Interrupted",
            };
            Some(Item::new("  ", muted).with(state, muted))
        }
        EventType::DecisionCall(call) => {
            Some(Item::new("? ", muted).with(summarize_decision_call(call), muted))
        }
        EventType::Model(config) => Some(Item::new("  ", muted).with(
            format!("Model {} · {}", config.model.slug(), config.effort.as_str()),
            muted,
        )),
        _ => None,
    }
}

/// A divider before a model turn, labeled with its number and usage.
fn describe_turn(
    transcript: &Transcript,
    payload: &RowPayload,
    options: &DisplayOptions,
    width: usize,
) -> Option<Item> {
    let mut label = match options.preferences.turn_dividers {
        TurnDividerDisplay::Hidden => return None,
        TurnDividerDisplay::Line => String::new(),
        TurnDividerDisplay::Labeled => {
            let mut parts = Vec::new();
            if let Some(turn) = transcript.turn_number(&payload.id()) {
                let bound = if turn.exact { "" } else { "≥" };
                parts.push(format!("turn {bound}{}", turn.number));
            }
            let end = transcript.inference_end(&payload.id());
            if let Some(EventType::InferenceResultEnd(end)) = end.as_ref().map(RowPayload::kind) {
                parts.push(format!(
                    "{} in · {} out",
                    describe_count(end.usage.input_tokens),
                    describe_count(end.usage.output_tokens)
                ));
                parts.push(format!("{:.1}s", end.request_time.as_secs_f64()));
            }
            format!(" {} ", parts.join(" · "))
        }
    };
    if label.trim().is_empty() {
        label.clear();
    }
    let rule_width = (width - 2).saturating_sub(label.chars().count() + 2);
    let text = format!("──{label}{}", "─".repeat(rule_width));
    Some(Item::new("  ", options.palette.muted).with(text, options.palette.muted))
}

fn describe_tool_call(
    transcript: &Transcript,
    payload: &RowPayload,
    call: &ToolCall,
    options: &DisplayOptions,
    width: usize,
) -> Item {
    let palette = options.palette;
    let summary = summarize_arguments(&call.arguments);
    let header = truncate_line(&format!("{} {summary}", call.tool_name), width - 2);
    let mut item =
        Item::new("● ", palette.tool).with(header, Style::new().add_modifier(Modifier::BOLD));
    if options.details && summary.len() < call.arguments.len() {
        item = item.with(call.arguments.clone(), palette.muted);
    }

    let result = transcript
        .tool_result(payload.agent_id(), &call.call_id)
        .and_then(|result| match result.kind() {
            EventType::ToolResult(result) => Some(result.clone()),
            _ => None,
        });
    let Some(result) = result else {
        return item.with("running…", palette.muted);
    };

    let limit = usize::from(options.preferences.tool_output_lines);
    let style = if result.error {
        palette.error
    } else {
        palette.muted
    };
    // Errors stay visible even when output is hidden.
    if limit == 0 && !result.error {
        return item;
    }
    let limit = limit.max(1);
    let output = content_text(&result.content);
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let mut shown = lines
        .iter()
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > limit {
        shown.push_str(&format!("\n… {} more lines", lines.len() - limit));
    }
    if shown.is_empty() {
        shown = "(no output)".into();
    }
    item.with(shown, style)
}

/// The most descriptive argument, or the compact arguments.
fn summarize_arguments(arguments: &str) -> String {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(arguments) else {
        return arguments.trim().to_owned();
    };
    SUMMARY_FIELDS
        .iter()
        .find_map(|name| fields.get(*name).and_then(|value| value.as_str()))
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| serde_json::Value::Object(fields).to_string())
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Refusal(refusal) => format!("Refused: {}", refusal.text),
            ContentBlock::Image(_) => "[image]".into(),
            ContentBlock::Audio(_) => "[audio]".into(),
            ContentBlock::File(_) => "[file]".into(),
            ContentBlock::ProviderContent(_) => "[provider content]".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Failed(error) => format!("Inference failed: {error}"),
        Outcome::Cancelled => "Inference cancelled".into(),
        Outcome::Incomplete { reason } => format!("Inference incomplete: {reason:?}"),
        Outcome::Completed { .. } | Outcome::InProgress => "Inference finished".into(),
    }
}

#[cfg(test)]
#[path = "../tests/tui/transcript_view/tests.rs"]
mod tests;
