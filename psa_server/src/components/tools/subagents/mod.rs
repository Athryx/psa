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

/// Optional fields can be absent, but Pi's non-nullable optionals reject null.
fn deserialize_present<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

async fn prepare(
    handle: &Option<SubagentCoordinator>,
    context: &AgentContext<'_>,
) -> Result<SubagentCoordinator, String> {
    let handle = handle
        .clone()
        .ok_or_else(|| "Subagents requires its session bundle".to_string())?;
    handle.ensure(context).await?;
    Ok(handle)
}
async fn finish(
    handle: &SubagentCoordinator,
    context: &mut AgentContext<'_>,
    result: Result<serde_json::Value, String>,
) -> ToolOutput {
    // Save identities even after partial startup failure. A durable child record
    // may exist when startup/steering fails and must remain addressable.
    if let Err(error) = handle.checkpoint(context).await {
        let message = format!("Subagents checkpoint failed: {error}; operation result: {result:?}");
        return ToolOutput::error(prompts::text(
            prompts::truncate_middle(&message, 50 * 1024).into_owned(),
        ));
    }
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
