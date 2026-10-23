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
struct CreateAgentArgs {
    name: String,
    task: String,
    /// Pinned permitted definition name. Missing or null requires a configured default.
    agent_template: Option<String>,
}
#[derive(Default)]
pub struct CreateAgent {
    handle: Option<SubagentCoordinator>,
}
impl CreateAgent {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self {
            handle: Some(handle),
        }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<CreateAgentArgs>(
            "create_agent",
            "Start a named child asynchronously. A null template requires a configured default. Names are workspace-unique.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: CreateAgentArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = match prepare(&self.handle, context).await {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let name = args.name.clone();
        let result = handle
            .create(
                context.agent().id,
                context.definition(),
                args.name,
                args.task,
                args.agent_template,
                cancellation,
            )
            .await;
        let succeeded = result.is_ok();
        let output = finish(&handle, context, result).await;
        if succeeded && !output.error {
            ToolOutput::success(prompts::text(format!("Agent {name} started.")))
        } else {
            output
        }
    }
}
impl Tool for CreateAgent {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
    }
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    typed_tool_execute!(CreateAgentArgs, execute_typed);
}
#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn typed_schema_accepts_optional_null_template_and_rejects_invalid_input() {
        for value in [
            json!({"name":"worker","task":"work"}),
            json!({"name":"worker","task":"work","agent_template":null}),
            json!({"name":"worker","task":"work","agent_template":"rust"}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_ok());
        }
        for value in [
            json!({"name":"worker"}),
            json!({"name":2,"task":"work"}),
            json!({"name":"worker","task":"work","extra":true}),
            json!({"name":"worker","task":"work","agent_template":false}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_err());
        }
        let parameters = serde_json::to_value(CreateAgent::definition().parameters).unwrap();
        assert_eq!(parameters["additionalProperties"], false);
    }
}
