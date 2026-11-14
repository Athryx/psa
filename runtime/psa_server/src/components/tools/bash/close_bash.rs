use super::{
    super::{CodeModeResultType, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute},
    bash::{ResultMode, backend_error, bash_result_type, format_result},
};
use crate::{
    agents::agent_context::AgentContext,
    components::{bundles::bash::BashHandle, cancellation::Cancellation},
};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloseBashArgs {
    /// Name of the background bash session to kill
    session_name: String,
}

pub struct CloseBash {
    handle: BashHandle,
}

impl CloseBash {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<CloseBashArgs>(
            "close_bash",
            "Kill a background bash session and its process tree. Returns output since the last poll (last 2000 lines / 50KB); its full output log is retained.",
            Some(false),
        )
    }

    pub(crate) fn with_handle(handle: BashHandle) -> Self {
        Self { handle }
    }

    async fn execute_typed(
        &self,
        arguments: CloseBashArgs,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = &self.handle;
        match handle.close(arguments.session_name).await {
            Ok(result) => format_result(result, ResultMode::Close),
            Err(error) => backend_error(error),
        }
    }
}

impl Tool for CloseBash {
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Stop a background bash session by name".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn codemode_result_type(&self) -> Option<CodeModeResultType> {
        Some(bash_result_type())
    }

    typed_tool_execute!(CloseBashArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn schema_and_arguments_match_reference() {
        let definition = CloseBash::definition();
        let schema = serde_json::to_value(definition.parameters).unwrap();
        assert_eq!(schema["required"], json!(["session_name"]));
        assert_eq!(schema["additionalProperties"], false);
        assert!(parse_tool_arguments::<CloseBashArgs>(json!({"session_name":"bash1"})).is_ok());
        for value in [
            json!({}),
            json!({"session_name":null}),
            json!({"session_name":1}),
            json!({"session_name":"bash1", "extra":true}),
        ] {
            assert!(parse_tool_arguments::<CloseBashArgs>(value).is_err());
        }
    }
}
