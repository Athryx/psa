use super::{finish, prepare};
use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::{
        bundles::subagents::SubagentCoordinator,
        cancellation::Cancellation,
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
    prompts,
    sessions::Session,
};
use llm_api::api::ToolDefinition;
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
#[derive(Default)]
pub struct ListAgents {
    handle: Option<SubagentCoordinator>,
}
impl ListAgents {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self {
            handle: Some(handle),
        }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<ListAgentsArgs>(
            "list_agents",
            "List direct children (including idle children). all=true lists your own agent group and parent relationships.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: ListAgentsArgs,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = match prepare(&self.handle, context).await {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let result = handle
            .list(context.agent().id, args.all.unwrap_or(false))
            .await;
        finish(&handle, context, result).await
    }
}
impl Tool for ListAgents {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
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
