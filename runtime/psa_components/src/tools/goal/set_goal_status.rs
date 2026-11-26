use crate::{
    bundles::goal::{GoalHandle, GoalOperation, GoalStatus},
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
struct SetGoalStatusArgs {
    status: GoalStatus,
    /// Explanation including evidence when completed or blocked
    #[serde(default, deserialize_with = "optional_reason")]
    #[schemars(with = "String")]
    reason: Option<String>,
}

// Type.Optional(Type.String()) permits omission, not an explicit null.
fn optional_reason<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

pub struct SetGoalStatus {
    handle: GoalHandle,
}

impl SetGoalStatus {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<SetGoalStatusArgs>(
            "set_goal_status",
            "Update an existing goal. For completed or blocked, supply a reason explaining strong evidence for that status; never invent evidence. Use running only when the user asks to resume a blocked/interrupted goal. Completed clears the goal.",
            Some(false),
        )
    }

    pub(crate) fn with_handle(handle: GoalHandle) -> Self {
        Self { handle }
    }

    async fn execute_typed(
        &self,
        arguments: SetGoalStatusArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = &self.handle;
        match handle
            .execute(
                GoalOperation::Status {
                    status: arguments.status,
                    reason: arguments.reason,
                },
                context,
                cancellation,
            )
            .await
        {
            Ok(message) => ToolOutput::success(prompts::text(message)),
            Err(error) => ToolOutput::error(prompts::text(error)),
        }
    }
}

impl Tool for SetGoalStatus {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::SetGoalStatus
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt { tools: None, rules: vec![
            "Call set_goal_status(completed or blocked) only with strong evidence, included in reason. Call set_goal_status(running) only when the user asks to resume a blocked/interrupted goal.".into(),
            "Do not declare a running goal finished without updating its status; if it is still running, keep working.".into(),
        ] }
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }
    typed_tool_execute!(SetGoalStatusArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn schema_and_typed_arguments_match_pi() {
        let definition = SetGoalStatus::definition();
        let schema = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "set_goal_status");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["status"]));
        assert_eq!(schema["properties"]["reason"]["type"], "string");
        for value in ["running", "completed", "blocked"] {
            assert!(parse_tool_arguments::<SetGoalStatusArgs>(json!({"status":value})).is_ok());
            assert!(
                parse_tool_arguments::<SetGoalStatusArgs>(
                    json!({"status":value,"reason":"evidence"})
                )
                .is_ok()
            );
        }
        for args in [
            json!({}),
            json!({"status":"unknown"}),
            json!({"status":1}),
            json!({"status":"running","reason":null}),
            json!({"status":"blocked","reason":true}),
            json!({"status":"completed","extra":true}),
        ] {
            assert!(parse_tool_arguments::<SetGoalStatusArgs>(args).is_err());
        }
    }
}
