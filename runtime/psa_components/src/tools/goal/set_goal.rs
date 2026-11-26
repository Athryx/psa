use crate::{
    bundles::goal::{GoalHandle, GoalOperation},
    prompts,
    tools::{ExecutionMode, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute},
};
use llm_api::api::ToolDefinition;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetGoalArgs {
    /// The user's explicit goal
    goal: String,
}

pub struct SetGoal {
    handle: GoalHandle,
}

impl SetGoal {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<SetGoalArgs>(
            "set_goal",
            "Set or replace the current goal and start running it. Call only when the user explicitly requests setting a goal.",
            Some(false),
        )
    }

    pub(crate) fn with_handle(handle: GoalHandle) -> Self {
        Self { handle }
    }

    async fn execute_typed(
        &self,
        arguments: SetGoalArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = &self.handle;
        match handle
            .execute(GoalOperation::Set(arguments.goal), context, cancellation)
            .await
        {
            Ok(message) => ToolOutput::success(prompts::text(message)),
            Err(error) => ToolOutput::error(prompts::text(error)),
        }
    }
}

impl Tool for SetGoal {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::SetGoal
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt { tools: None, rules: vec!["Call set_goal only when the user explicitly asks to set a goal; do not turn ordinary requests into goals.".into()] }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }
    typed_tool_execute!(SetGoalArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn schema_and_typed_arguments_match_pi() {
        let definition = SetGoal::definition();
        let schema = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "set_goal");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["goal"]));
        assert_eq!(schema["properties"]["goal"]["type"], "string");
        assert!(parse_tool_arguments::<SetGoalArgs>(json!({"goal":"explicit"})).is_ok());
        for args in [
            json!({}),
            json!({"goal":null}),
            json!({"goal":1}),
            json!({"goal":"x","extra":true}),
        ] {
            assert!(parse_tool_arguments::<SetGoalArgs>(args).is_err());
        }
    }
}
