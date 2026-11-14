//! Plain-text rendering of events for people and agents reading traces.

use crate::model::{Event, EventKind, ImageRef, Inference, ToolResult};
use std::fmt::Write;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Middle-truncate each event body to this many characters; `None` shows all.
    pub max_chars: Option<usize>,
    pub show_time: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            max_chars: Some(2000),
            show_time: false,
        }
    }
}

/// Render one event as `#id type ...: body`, with the body indented below.
pub fn render_event(event: &Event, options: RenderOptions) -> String {
    let mut out = format!("#{}", event.id);
    if options.show_time
        && let Some(t) = event.t
        && let Ok(formatted) = t.format(&Rfc3339)
    {
        let _ = write!(out, " [{formatted}]");
    }
    let _ = write!(out, " {}", event.kind.type_name());

    let (header, body) = describe(&event.kind, options.max_chars);
    if !header.is_empty() {
        let _ = write!(out, " {header}");
    }
    if body.is_empty() {
        return out;
    }

    out.push(':');
    if body.contains('\n') {
        for line in body.lines() {
            out.push_str("\n    ");
            out.push_str(line);
        }
    } else {
        out.push(' ');
        out.push_str(&body);
    }
    out
}

/// Header annotations and the main text of an event, each text part truncated to
/// `max_chars`.
fn describe(kind: &EventKind, max_chars: Option<usize>) -> (String, String) {
    let clip = |text: String| match max_chars {
        Some(max) => truncate_middle(&text, max),
        None => text,
    };
    match kind {
        EventKind::System { text }
        | EventKind::Assistant { text }
        | EventKind::Thinking { text } => (String::new(), clip(text.clone())),
        EventKind::User { text, images } => (String::new(), clip(with_images(text, images))),
        EventKind::ToolCall { name, args, result } => {
            let args = match args {
                serde_json::Value::String(raw) => raw.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            let mut header = name.clone();
            let mut body = clip(args);
            let Some(result) = result else {
                header.push_str(" no result");
                return (header, body);
            };
            append_result_status(&mut header, result);
            body.push_str("\n-> ");
            body.push_str(&clip(with_images(&result.output, &result.images)));
            (header, body)
        }
        EventKind::Context { kind, text } => (format!("({kind})"), clip(text.clone())),
        EventKind::AgentMessage { from, to, text } => {
            (format!("{from} -> {to}"), clip(text.clone()))
        }
        EventKind::Compaction {
            summary,
            tokens_before,
        } => {
            let header = tokens_before
                .map(|tokens| format!("(from {tokens} tokens)"))
                .unwrap_or_default();
            (header, clip(summary.clone()))
        }
        EventKind::Inference(inference) => (describe_inference(inference), String::new()),
        EventKind::ModelChange { model, effort } => {
            let mut header = model.clone().unwrap_or_default();
            if let Some(effort) = effort {
                let _ = write!(header, " effort={effort}");
            }
            (header.trim().to_owned(), String::new())
        }
        EventKind::AgentFinish {
            outcome,
            reason,
            duration_ms,
        } => {
            let mut header = outcome.as_str().to_owned();
            if let Some(ms) = duration_ms {
                let _ = write!(header, " {}", format_duration(*ms));
            }
            if let Some(reason) = reason {
                let _ = write!(header, " ({reason})");
            }
            (header, String::new())
        }
    }
}

fn append_result_status(header: &mut String, result: &ToolResult) {
    if result.is_error {
        header.push_str(" error");
    }
    if let Some(ms) = result.duration_ms {
        let _ = write!(header, " {}", format_duration(ms));
    }
}

fn describe_inference(inference: &Inference) -> String {
    let mut parts = Vec::new();
    if let Some(model) = &inference.model {
        parts.push(model.clone());
    }
    if let Some(input) = inference.input_tokens {
        let mut part = format!("in={input}");
        if let Some(cached) = inference.cached_input_tokens.filter(|n| *n > 0) {
            let _ = write!(part, " (cached {cached})");
        }
        parts.push(part);
    }
    if let Some(output) = inference.output_tokens {
        let mut part = format!("out={output}");
        if let Some(reasoning) = inference.reasoning_tokens.filter(|n| *n > 0) {
            let _ = write!(part, " (reasoning {reasoning})");
        }
        parts.push(part);
    }
    if let Some(cost) = inference.cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    if let Some(ms) = inference.duration_ms {
        parts.push(format_duration(ms));
    }
    if let Some(stop) = &inference.stop_reason {
        parts.push(format!("stop={stop}"));
    }
    if let Some(error) = &inference.error {
        parts.push(format!("error={error}"));
    }
    parts.join(" ")
}

fn with_images(text: &str, images: &[ImageRef]) -> String {
    let mut out = text.to_owned();
    for image in images {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "[image {} blobs/{}]", image.mime, image.blob);
    }
    out
}

pub fn format_duration(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000),
        _ => format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000),
    }
}

/// Keep the start and end of `text`, replacing the middle with an omission marker.
/// Counts characters, so multi-byte text is never split.
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head = max_chars.div_ceil(2);
    let tail = max_chars - head;
    let omitted = total - head - tail;

    let mut out: String = text.chars().take(head).collect();
    let _ = write!(out, "\n[... {omitted} chars omitted ...]\n");
    out.extend(text.chars().skip(total - tail));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FinishOutcome;
    use serde_json::json;

    #[test]
    fn renders_tool_calls_with_their_results() {
        let call = |result| Event {
            id: 3,
            ..Event::new(
                None,
                EventKind::ToolCall {
                    name: "bash".into(),
                    args: json!({"command": "ls"}),
                    result,
                },
            )
        };
        assert_eq!(
            render_event(&call(None), RenderOptions::default()),
            "#3 tool_call bash no result: {\"command\":\"ls\"}"
        );

        let result = ToolResult {
            output: "a\nb".into(),
            is_error: true,
            duration_ms: Some(12),
            ..Default::default()
        };
        assert_eq!(
            render_event(&call(Some(result)), RenderOptions::default()),
            "#3 tool_call bash error 12ms:\n    {\"command\":\"ls\"}\n    -> a\n    b"
        );

        let end = Event::new(
            None,
            EventKind::AgentFinish {
                outcome: FinishOutcome::Aborted,
                reason: Some("interrupted".into()),
                duration_ms: Some(65_000),
            },
        );
        assert_eq!(
            render_event(&end, RenderOptions::default()),
            "#0 agent_finish aborted 1m05s (interrupted)"
        );
    }

    #[test]
    fn middle_truncation_keeps_both_ends_and_unicode() {
        let text = "ééééé-middle-ààààà";
        let truncated = truncate_middle(text, 10);
        assert!(truncated.starts_with("ééééé"));
        assert!(truncated.ends_with("ààààà"));
        assert!(truncated.contains("8 chars omitted"));
        assert_eq!(truncate_middle("short", 10), "short");
    }
}
