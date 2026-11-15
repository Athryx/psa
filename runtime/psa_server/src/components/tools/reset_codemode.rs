use super::{ExecutionMode, Tool, ToolOutput, typed_tool_execute};
use crate::{
    agents::agent_context::AgentContext,
    components::{bundles::codemode::MontyEngine, cancellation::Cancellation},
    db::PrivateEvent,
    prompts,
};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResetCodeModeArgs {}

pub struct ResetCodeMode {
    engine: Arc<MontyEngine>,
}

impl ResetCodeMode {
    pub(crate) fn new(engine: Arc<MontyEngine>) -> Self {
        Self { engine }
    }

    pub(super) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<ResetCodeModeArgs>(
            "reset_codemode",
            "Start a fresh codemode Python REPL when the current one is unusable. Conversation history and external effects are unaffected.",
            Some(false),
        )
    }

    async fn execute_typed(
        &self,
        _arguments: ResetCodeModeArgs,
        _context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let snapshot = match self.engine.reset(&mut cancellation).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let message = format!("Reset failed; REPL state is unchanged. {error}");
                return ToolOutput::error(prompts::text(prompts::truncate_middle(
                    &message, 16_000,
                )));
            }
        };

        let mut output = ToolOutput::success(prompts::text(
            "Python REPL reset. Earlier codemode outputs remain available through prior_results.",
        ));
        output
            .private_events
            .push(PrivateEvent::CodeModeSnapshot(snapshot));
        output
    }
}

impl Tool for ResetCodeMode {
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    typed_tool_execute!(ResetCodeModeArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn definition_has_a_closed_empty_object_schema() {
        let definition = ResetCodeMode::definition();
        assert_eq!(definition.name, "reset_codemode");
        assert_eq!(definition.strict, Some(false));
        let schema = definition.parameters.as_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema
                .get("properties")
                .is_none_or(|value| value == &json!({}))
        );
        assert!(
            definition
                .parameters
                .validate_openai_parameters(definition.strict)
                .is_ok()
        );
    }

    #[test]
    fn typed_arguments_require_an_empty_object() {
        assert!(parse_tool_arguments::<ResetCodeModeArgs>(json!({})).is_ok());
        for arguments in [
            json!({"code": "pass"}),
            json!(null),
            json!([]),
            json!("reset"),
        ] {
            assert!(parse_tool_arguments::<ResetCodeModeArgs>(arguments).is_err());
        }
    }

    #[test]
    fn reset_is_sequential_and_uses_bounded_cancellation_grace() {
        let tool = ResetCodeMode::new(Arc::new(MontyEngine::new("monty")));
        assert_eq!(tool.execution_mode(), ExecutionMode::Sequential);
        assert_eq!(tool.cancellation_timeout(), Duration::from_secs(3));
    }
}
