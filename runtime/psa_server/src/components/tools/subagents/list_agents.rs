use super::{format_tool_output, require_coordinator};
use crate::{
    agents::agent_context::AgentContext,
    components::{
        bundles::subagents::SubagentCoordinator,
        cancellation::Cancellation,
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
    prompts,
};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
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
            "List direct children (including idle children). all=true lists every agent in this session with parent relationships.",
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

    async fn list_agents(context: &AgentContext<'_>, all: bool) -> Result<Value, String> {
        let agents = context
            .sessions()
            .list_agents(context.session().id)
            .await
            .map_err(|error| error.to_string())?;
        let mut members = Vec::new();
        for agent in agents
            .iter()
            .filter(|agent| all || agent.subagent_parent_id == Some(context.agent().id))
        {
            let definition = context
                .sessions()
                .get_agent_definition(agent.definition_id)
                .await
                .map_err(|error| error.to_string())?;
            let parent = agent
                .subagent_parent_id
                .and_then(|id| agents.iter().find(|agent| agent.id == id));
            members.push(json!({
                "agent_id": agent.id,
                "name": agent.name,
                "parent_id": agent.subagent_parent_id,
                "parent": parent.map(|agent| &agent.name),
                "template": definition.name,
                "interactive": agent.interactive,
                "hidden": agent.hidden,
                "status": agent.state,
            }));
        }
        members.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(Value::Array(members))
    }
}
impl Tool for ListAgents {
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
