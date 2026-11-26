//! Model-facing tools for the session-owned Subagents bundle.
mod create_agent;
mod list_agents;
mod send_message;
mod wait_message;
pub use create_agent::CreateAgent;
pub use list_agents::ListAgents;
pub use send_message::SendMessage;
pub use wait_message::WaitMessage;

use crate::{bundles::subagents::SubagentCoordinator, prompts, tools::ToolOutput};
use psa::api::events::AgentMessageLink;
use psa_agents::agent_context::AgentContext;

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

/// Like [`format_tool_output`], recording the delivered message for clients
/// without showing its IDs to the model.
fn format_delivery_output(result: Result<(String, AgentMessageLink), String>) -> ToolOutput {
    match result {
        Ok((text, delivery)) => {
            let mut output = format_tool_output(Ok(text));
            output.delivery = Some(delivery);
            output
        }
        Err(error) => format_tool_output(Err(error)),
    }
}

fn format_tool_output(result: Result<String, String>) -> ToolOutput {
    match result {
        Ok(text) => ToolOutput::success(prompts::text(
            prompts::truncate_middle(&text, 50 * 1024).into_owned(),
        )),
        Err(error) => ToolOutput::error(prompts::text(
            prompts::truncate_middle(&error, 50 * 1024).into_owned(),
        )),
    }
}
