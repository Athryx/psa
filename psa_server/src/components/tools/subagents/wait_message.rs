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
struct WaitMessageArgs {
    /// Seconds, greater than zero and at most 600; defaults to 120.
    #[serde(default, deserialize_with = "super::deserialize_present")]
    #[schemars(schema_with = "timeout_schema")]
    timeout: Option<f64>,
}
fn timeout_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"number","exclusiveMinimum":0,"maximum":600})
}
#[derive(Default)]
pub struct WaitMessage {
    handle: Option<SubagentCoordinator>,
}
impl WaitMessage {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self {
            handle: Some(handle),
        }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<WaitMessageArgs>(
            "wait_message",
            "Wait for a message in your agent group; timeout is seconds, default 120, maximum 600. Refuses when nobody else can make progress.",
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
        let handle = match prepare(&self.handle, context).await {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let result = handle.wait(context, timeout, cancellation).await;
        finish(&handle, context, result).await
    }
}
impl Tool for WaitMessage {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self::default()
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
