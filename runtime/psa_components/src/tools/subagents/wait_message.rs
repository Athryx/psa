use super::{format_tool_output, require_coordinator};
use crate::{
    bundles::subagents::SubagentCoordinator,
    prompts,
    tools::{Tool, ToolOutput, typed_tool_execute},
};
use llm_api::api::ToolDefinition;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitMessageArgs {
    /// Seconds, greater than zero and at most 600; defaults to 120.
    #[serde(default, deserialize_with = "super::deserialize_present")]
    #[schemars(schema_with = "timeout_schema")]
    timeout: Option<f64>,
}
fn timeout_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"number","exclusiveMinimum":0,"maximum":600})
}
pub struct WaitMessage {
    handle: SubagentCoordinator,
}
impl WaitMessage {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self { handle }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<WaitMessageArgs>(
            "wait_message",
            "Wait for a message from another agent or the user, such as a child's final answer. timeout is in seconds (default 120, maximum 600). Fails immediately when no other agent is running, since no message could arrive.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: WaitMessageArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let timeout = args.timeout.unwrap_or(120.0);
        if !timeout.is_finite() || timeout <= 0.0 || timeout > 600.0 {
            return ToolOutput::error(prompts::text("timeout must be >0 and <=600 seconds"));
        }
        let handle = match require_coordinator(&self.handle, context) {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let result = handle.wait(context, timeout, cancellation).await;
        format_tool_output(result)
    }
}
impl Tool for WaitMessage {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::WaitMessage
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    typed_tool_execute!(WaitMessageArgs, execute_typed);
}
#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn optional_seconds_and_closed_schema() {
        assert!(parse_tool_arguments::<WaitMessageArgs>(json!({})).is_ok());
        assert!(parse_tool_arguments::<WaitMessageArgs>(json!({"timeout":2.5})).is_ok());
        assert!(parse_tool_arguments::<WaitMessageArgs>(json!({"timeout":"2"})).is_err());
        assert!(parse_tool_arguments::<WaitMessageArgs>(json!({"timeout":null})).is_err());
        assert!(parse_tool_arguments::<WaitMessageArgs>(json!({"unexpected":1})).is_err());
        let parameters = serde_json::to_value(WaitMessage::definition().parameters).unwrap();
        assert_eq!(parameters["properties"]["timeout"]["maximum"], 600);
        assert_eq!(parameters["properties"]["timeout"]["exclusiveMinimum"], 0);
    }
}
