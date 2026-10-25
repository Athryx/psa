//! Model-facing tools for the session-owned Subagents bundle.
mod create_agent;
mod list_agents;
mod send_message;
mod wait_message;
pub use create_agent::CreateAgent;
pub use list_agents::ListAgents;
pub use send_message::SendMessage;
pub use wait_message::WaitMessage;

use crate::{
    agents::agent_context::AgentContext,
    components::{bundles::subagents::SubagentCoordinator, tools::ToolOutput},
    prompts,
};

/// Optional fields can be absent, but explicitly supplied null is invalid.
fn deserialize_present<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn require_coordinator(
    handle: &SubagentCoordinator,
    context: &AgentContext<'_>,
) -> Result<SubagentCoordinator, String> {
    handle.require_session(context.session().id)?;
    Ok(handle.clone())
}

fn format_tool_output(result: Result<serde_json::Value, String>) -> ToolOutput {
    match result {
        Ok(value) => ToolOutput::success(prompts::text(
            prompts::truncate_middle(&serde_json::to_string_pretty(&value).unwrap(), 50 * 1024)
                .into_owned(),
        )),
        Err(error) => ToolOutput::error(prompts::text(
            prompts::truncate_middle(&error, 50 * 1024).into_owned(),
        )),
    }
}
