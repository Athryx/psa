//! Shared harness prompts and model-facing formatting utilities.
//! Tool-specific prompt text belongs in the corresponding tool's own file.
use llm_api::api::{ContentBlock, TextBlock};
use std::{borrow::Cow, path::Path};

#[cfg(test)]
#[path = "tests/prompts/tests.rs"]
mod tests;

/// Bounds model-facing text by bytes, retaining its beginning and end.
///
/// The budget includes the truncation marker. UTF-8 characters are never split;
/// very small budgets use a shortened ASCII marker. Unchanged text is borrowed.
pub fn truncate_middle(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }

    const MARKER: &str = "\n[... middle truncated ...]\n";
    let marker = if max_bytes >= MARKER.len() {
        MARKER
    } else {
        "..."
    };
    if max_bytes < marker.len() {
        return Cow::Borrowed(&marker[..max_bytes]);
    }

    let retained = max_bytes - marker.len();
    let mut prefix_end = retained.div_ceil(2);
    while !text.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut suffix_start = text.len() - retained / 2;
    while !text.is_char_boundary(suffix_start) {
        suffix_start += 1;
    }

    Cow::Owned(format!(
        "{}{marker}{}",
        &text[..prefix_end],
        &text[suffix_start..]
    ))
}

pub fn default_system_prompt() -> &'static str {
    "You are an expert coding assistant operating inside psa, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files."
}

pub fn cwd_system_prompt(cwd: &Path) -> String {
    format!("<cwd>\n{}\n</cwd>", escape_xml(&cwd.to_string_lossy()))
}

pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn text(content: impl Into<String>) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextBlock {
        text: content.into(),
        annotations: Vec::new(),
    })]
}

/// Acknowledgement of a recorded result, also used when recovery repairs it.
pub fn result_recorded() -> Vec<ContentBlock> {
    text("Result recorded.")
}

pub fn tool_interrupted() -> Vec<ContentBlock> {
    text("Tool execution was interrupted; its outcome is unknown and it may have had side effects.")
}
pub fn tool_timed_out() -> Vec<ContentBlock> {
    text("Tool cancellation timed out; its outcome is unknown and it may have had side effects.")
}
pub fn tool_not_run() -> Vec<ContentBlock> {
    text("Tool was not run because execution was interrupted.")
}
pub fn tool_invalid_arguments() -> Vec<ContentBlock> {
    text("Tool arguments are invalid for this tool.")
}
pub fn tool_argument_error(detail: &str) -> Vec<ContentBlock> {
    let detail = detail
        .strip_prefix("invalid tool arguments ")
        .unwrap_or(detail);
    let message =
        format!("Invalid tool arguments: {detail}. Correct the indicated field and retry.");
    text(truncate_middle(&message, 2048).into_owned())
}
pub fn tool_argument_json_error(line: usize, column: usize) -> Vec<ContentBlock> {
    text(format!(
        "Invalid tool arguments: expected a JSON object; malformed JSON at line {line}, column {column}. Correct the JSON and retry."
    ))
}
pub fn tool_unknown() -> Vec<ContentBlock> {
    text("The requested tool is not available.")
}
pub fn tool_failed() -> Vec<ContentBlock> {
    text("Tool execution failed.")
}
/// Header for a message from another agent. Agents address each other by
/// name; internal IDs are not model-facing.
pub fn agent_message_header(sender_name: &str) -> String {
    format!("Message from agent \"{sender_name}\":\n")
}
pub fn context_summary(summary: &str) -> String {
    format!("Previous context summary:\n{summary}")
}

pub fn incomplete_tool_call(name: &str, arguments: &str) -> Vec<ContentBlock> {
    text(format!(
        "An interrupted, incomplete tool call to {name} cannot be replayed as an executable call. Captured arguments: {arguments}\n"
    ))
}
