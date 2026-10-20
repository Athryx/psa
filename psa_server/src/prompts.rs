//! All harness-authored text sent to the model lives here, including tool errors.
use llm_api::api::{ContentBlock, TextBlock};

pub fn text(content: impl Into<String>) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextBlock {
        text: content.into(),
        annotations: Vec::new(),
    })]
}

pub fn noop_description() -> String {
    "A placeholder tool that does nothing.".into()
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
pub fn tool_unknown() -> Vec<ContentBlock> {
    text("The requested tool is not available.")
}
pub fn tool_failed() -> Vec<ContentBlock> {
    text("Tool execution failed.")
}
pub fn context_summary(summary: &str) -> String {
    format!("Previous context summary:\n{summary}")
}

pub fn incomplete_tool_call(name: &str, arguments: &str) -> Vec<ContentBlock> {
    text(format!(
        "An interrupted, incomplete tool call to {name} cannot be replayed as an executable call. Captured arguments: {arguments}\n"
    ))
}
