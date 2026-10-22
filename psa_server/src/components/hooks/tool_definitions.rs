use super::{Hook, HookError};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::{
        cancellation::Cancellation,
        tools::{Tool, ToolSystemPrompt},
    },
    prompts,
    sessions::Session,
};
use llm_api::api::ToolDefinition as ApiToolDefinition;
use psa_api::{
    ToolId,
    events::{EventType, Message, MessageType, NewEvent, ToolDefinition},
};
use std::time::Duration;

/// Snapshots tool schemas and their optional tools/rules system-prompt sections.
pub struct ToolDefinitionsHook;

impl Hook for ToolDefinitionsHook {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> Result<(), HookError> {
        // Forks retain their inherited snapshots; adding them again is invalid.
        if context.agent().parent_id.is_some() {
            return Ok(());
        }

        let events = build_tool_events(
            context
                .tools()
                .iter()
                .map(|tool| (tool.id(), tool.tool_definition(), tool.system_prompt())),
        );
        context.save_events(&events).await?;
        Ok(())
    }
}

fn build_tool_events(
    tools: impl IntoIterator<Item = (ToolId, ApiToolDefinition, ToolSystemPrompt)>,
) -> Vec<NewEvent> {
    let mut events = Vec::new();
    let mut directions = Vec::new();
    let mut rules = Vec::new();
    for (id, definition, prompt) in tools {
        if let Some(direction) = prompt.tools {
            directions.push((definition.name.clone(), direction));
        }
        rules.extend(prompt.rules);
        events.push(NewEvent::new(EventType::ToolDefinition(ToolDefinition {
            id,
            definition,
        })));
    }

    for section in [
        prompts::tools::format_tools(directions),
        prompts::tools::format_rules(rules),
    ]
    .into_iter()
    .flatten()
    {
        events.push(NewEvent::new(EventType::Message(Message::text(
            MessageType::System,
            section,
        ))));
    }
    events
}

#[cfg(test)]
mod tests;
