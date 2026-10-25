use super::{
    super::{Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute},
    bash::{ResultMode, backend_error, format_result, optional_value, yield_duration},
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
struct InteractBashArgs {
    /// Name of the background bash session
    session_name: String,
    /// Exact text to write to stdin (no newline added); omit or use empty string to just poll output
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(with = "String")]
    stdin: Option<String>,
    /// Wait up to this many milliseconds (default 1000 when sending stdin, 10000 when polling, max 300000)
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(with = "f64")]
    yield_time_ms: Option<f64>,
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(skip)]
    yield_timeout_ms: Option<f64>,
}

pub struct InteractBash {
    handle: BashHandle,
}

impl InteractBash {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<InteractBashArgs>(
            "interact_bash",
            "Send text to a running bash session's stdin or poll for new stdout/stderr. Returns only output since the previous call (last 2000 lines / 50KB); full combined output stays in its log. Waits up to yield_time_ms (max 300000); defaults to 1000ms when sending stdin, 10000ms when polling.",
            Some(false),
        )
    }

    pub(crate) fn with_handle(handle: BashHandle) -> Self {
        Self { handle }
    }

    async fn execute_typed(
        &self,
        arguments: InteractBashArgs,
        _context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let fallback = if arguments
            .stdin
            .as_ref()
            .is_some_and(|text| !text.is_empty())
        {
            1000
        } else {
            10000
        };
        let wait = match yield_duration(
            arguments.yield_time_ms.or(arguments.yield_timeout_ms),
            fallback,
        ) {
            Ok(wait) => wait,
            Err(error) => return error,
        };
        let handle = &self.handle;
        match handle
            .interact(arguments.session_name, arguments.stdin, wait, cancellation)
            .await
        {
            Ok(result) => format_result(result, ResultMode::Interact),
            Err(error) => backend_error(error),
        }
    }
}

impl Tool for InteractBash {
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Send stdin to or poll a background bash session for new output".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    typed_tool_execute!(InteractBashArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn schema_and_arguments_match_reference() {
        let definition = InteractBash::definition();
        let schema = serde_json::to_value(definition.parameters).unwrap();
        assert_eq!(schema["required"], json!(["session_name"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["stdin"]["type"], "string");
        assert_eq!(schema["properties"]["yield_time_ms"]["type"], "number");
        assert!(schema["properties"].get("yield_timeout_ms").is_none());
        for value in [
            json!({"session_name":"bash1"}),
            json!({"session_name":"bash1", "stdin":""}),
            json!({"session_name":"bash1", "stdin":"hello\n", "yield_time_ms":0}),
            json!({"session_name":"bash1", "yield_timeout_ms":1000}),
        ] {
            assert!(parse_tool_arguments::<InteractBashArgs>(value).is_ok());
        }
        for value in [
            json!({}),
            json!({"session_name":1}),
            json!({"session_name":"bash1", "stdin":null}),
            json!({"session_name":"bash1", "yield_time_ms":null}),
            json!({"session_name":"bash1", "yield_timeout_ms":null}),
            json!({"session_name":"bash1", "other":1}),
        ] {
            assert!(parse_tool_arguments::<InteractBashArgs>(value).is_err());
        }
    }
}
