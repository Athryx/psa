use super::{format_tool_output, require_coordinator};
use crate::{
    bundles::subagents::SubagentCoordinator,
    prompts,
    tools::{Tool, ToolOutput, typed_tool_execute},
};
use llm_api::api::ToolDefinition;
use psa::api::AgentState;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    #[serde(default, deserialize_with = "super::deserialize_present")]
    #[schemars(schema_with = "bool_schema")]
    all: Option<bool>,
}
fn bool_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"boolean"})
}
pub struct ListAgents {
    handle: SubagentCoordinator,
}
impl ListAgents {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self { handle }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<ListAgentsArgs>(
            "list_agents",
            "List your child agents and their status (running, idle, or interrupted). all=true lists every agent in this session with its parent.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: ListAgentsArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let _handle = match require_coordinator(&self.handle, context) {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err("Agent listing cancelled".into()),
            result = Self::list_agents(context, args.all.unwrap_or(false)) => result,
        };
        format_tool_output(result)
    }

    async fn list_agents(context: &AgentContext<'_>, all: bool) -> Result<String, String> {
        let agents = context
            .runtime()
            .list_agents(context.session().id)
            .await
            .map_err(|error| error.to_string())?;
        let caller = context.agent().id;
        let mut lines = Vec::new();
        for agent in agents
            .iter()
            .filter(|agent| all || agent.subagent_parent_id == Some(caller))
        {
            let definition = context
                .runtime()
                .get_agent_definition(agent.definition_id)
                .await
                .map_err(|error| error.to_string())?;
            let status = match agent.state {
                AgentState::Idle => "idle",
                AgentState::Interrupted => "interrupted",
                AgentState::Running => "running",
            };
            let mut details = vec![format!("template {}", definition.name)];
            if all {
                let parent = agent
                    .subagent_parent_id
                    .and_then(|id| agents.iter().find(|agent| agent.id == id));
                match parent {
                    Some(parent) if parent.id == caller => details.push("your child".into()),
                    Some(parent) => details.push(format!("child of {}", parent.name)),
                    None => details.push("top-level".into()),
                }
            }
            let you = if agent.id == caller { " (you)" } else { "" };
            lines.push((
                agent.name.clone(),
                format!("- {}{you}: {status}; {}", agent.name, details.join(", ")),
            ));
        }
        if lines.is_empty() {
            return Ok(if all {
                "No agents in this session.".into()
            } else {
                "You have no child agents.".into()
            });
        }
        lines.sort();
        Ok(lines
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

impl Tool for ListAgents {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::ListAgents
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    typed_tool_execute!(ListAgentsArgs, execute_typed);
}
#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn optional_boolean_and_closed_schema() {
        for value in [json!({}), json!({"all":true}), json!({"all":false})] {
            assert!(parse_tool_arguments::<ListAgentsArgs>(value).is_ok());
        }
        for value in [
            json!({"all":"true"}),
            json!({"all":null}),
            json!({"other":true}),
        ] {
            assert!(parse_tool_arguments::<ListAgentsArgs>(value).is_err());
        }
    }
}
