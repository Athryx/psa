use crate::{
    agents::{Agent, agent_context::AgentContext},
    components::{
        bundles::goal::{GoalHandle, GoalOperation},
        cancellation::Cancellation,
        tools::{ExecutionMode, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute},
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
struct SetGoalArgs {
    /// The user's explicit goal
    goal: String,
}

#[derive(Default)]
pub struct SetGoal {
    handle: Option<GoalHandle>,
}

impl SetGoal {
    pub(crate) fn with_handle(handle: GoalHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn execute_typed(
        &self,
        arguments: SetGoalArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let Some(handle) = &self.handle else {
            return ToolOutput::error(prompts::text("set_goal requires the Goal bundle"));
        };
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
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
    }
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::for_type::<SetGoalArgs>(
            "set_goal",
            "Set or replace the current goal and start running it. Call only when the user explicitly requests setting a goal.",
            Some(false),
        )
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
        let definition = SetGoal::default().tool_definition();
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
        assert!(matches!(
            SetGoal::default().execution_mode(),
            ExecutionMode::Sequential
        ));
    }
}
