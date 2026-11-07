use super::{ExecutionMode, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};
use crate::{
    agents::agent_context::AgentContext,
    components::{
        bundles::codemode::MontyEngine,
        cancellation::Cancellation,
        codemode_functions::{CodeModeFunction, CodeModeFunctionKind},
    },
    db::PrivateEvent,
    prompts,
};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CodeModeArgs {
    /// Python code to execute in the agent's persistent REPL.
    code: String,
}

pub struct CodeMode {
    engine: Arc<MontyEngine>,
}

impl CodeMode {
    pub(crate) fn new(engine: Arc<MontyEngine>) -> Self {
        Self { engine }
    }

    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<CodeModeArgs>(
            "codemode",
            "Execute Python in a persistent, sandboxed agent REPL. Use print() to return output. Registered functions are available as async functions; await them. State is checkpointed between calls.",
            Some(false),
        )
    }

    async fn execute_typed(
        &self,
        arguments: CodeModeArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let result = self
            .engine
            .execute(&arguments.code, context, &mut cancellation)
            .await;
        let mut output = if result.error {
            ToolOutput::error(prompts::text(result.text))
        } else {
            ToolOutput::success(prompts::text(result.text))
        };
        if let Some(snapshot) = result.snapshot {
            output
                .private_events
                .push(PrivateEvent::CodeModeSnapshot(snapshot));
        }
        output
    }
}

impl Tool for CodeMode {
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Run Monty's Python subset in a persistent REPL; it has no direct filesystem or network access. Use print() to return output; the final expression is not returned automatically. Printed output is bounded and may be truncated in the middle. Call registered functions with await and keyword arguments. Tool-backed functions return text strings on success and raise exceptions on failure. Use asyncio.gather for independent parallel calls. Each script has a 660-second execution limit once it starts; time waiting for earlier calls is not included. Uncaught exceptions make the call fail. REPL state can reset after interruption or an incompatible checkpoint; external side effects are not rolled back.".into()),
            rules: vec![],
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(12)
    }

    typed_tool_execute!(CodeModeArgs, execute_typed);
}

/// Render the installed Python functions without publishing native tool definitions.
pub(crate) fn format_codemode_functions(functions: &[CodeModeFunctionKind]) -> String {
    let mut text =
        String::from("Available codemode functions (call with await and keyword arguments):\n");
    for function in functions {
        let definition = function.definition();
        text.push_str(&format!(
            "- {}: {}\n  Arguments (JSON Schema): {}\n",
            definition.name,
            definition.description,
            definition.parameters.as_value(),
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::tools::{Noop, ToolKind};
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn typed_arguments_require_only_code() {
        assert!(parse_tool_arguments::<CodeModeArgs>(json!({"code": "print(1)"})).is_ok());
        for value in [
            json!({}),
            json!({"code": null}),
            json!({"code": 1}),
            json!({"code": "x", "extra": true}),
        ] {
            assert!(parse_tool_arguments::<CodeModeArgs>(value).is_err());
        }
    }

    #[test]
    fn system_prompt_describes_function_results_and_output_limits() {
        let prompt = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        }
        .system_prompt();
        let directions = prompt.tools.unwrap();
        assert!(directions.contains("text strings on success"));
        assert!(directions.contains("raise exceptions on failure"));
        assert!(directions.contains("may be truncated"));
    }

    #[test]
    fn codemode_is_scheduled_sequentially() {
        let tool = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        };
        assert_eq!(tool.execution_mode(), ExecutionMode::Sequential);
    }

    #[test]
    fn registered_function_guidance_contains_names_and_schema() {
        let functions = [CodeModeFunctionKind::Tool(ToolKind::Noop(Noop))];
        let text = format_codemode_functions(&functions);
        assert!(text.contains("noop:"));
        assert!(text.contains("JSON Schema"));
        assert!(text.contains("additionalProperties"));
    }
}
