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
            "Execute Python in a persistent Monty sandbox. Use print() for output, await tool-backed functions, and call PRIOR_RESULTS() without await.",
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
            tools: Some(concat!(
                "Persistent Monty Python subset, not full CPython. Python's filesystem/network APIs are sandboxed; installed tool functions can access external resources. ",
                "globals(), dir(), vars(), and __import__() are unavailable.\n\n",
                "Use print() for output; final expressions aren't displayed. Output may be middle-truncated. ",
                "Await tool-backed functions with keyword arguments; they return text or raise exceptions. ",
                "Use asyncio.gather for independent calls, and finish all awaits before returning. ",
                "Call PRIOR_RESULTS(index=-1) for previous codemode output.\n\n",
                "Limits: 60s interpreter execution excluding host waits, 660s wall, 64 MiB execution memory, 16 MiB serialized checkpoint. ",
                "Uncaught exceptions can preserve earlier changes. Cancellation, resource limits, and checkpoint failures keep the previous checkpoint. ",
                "External effects are not rolled back."
            ).into()),
            rules: vec![concat!(
                "Use PRIOR_RESULTS(-1) to slice, search, or compute a short excerpt before printing, ",
                "e.g. print(PRIOR_RESULTS(-1)[2000:4000]). It cannot recover truncated-away content; ",
                "inspect retained REPL data or use a narrower read-only query instead. Never repeat side effects just to recover output."
            ).into(), "If codemode gets into a bad state, call reset_codemode directly (not inside Python). This clears Python state, not history or external effects.".into()],
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
    let mut text = String::from("Available codemode functions:\n");
    for function in functions {
        let definition = function.definition();
        let calling_convention = match function {
            CodeModeFunctionKind::Tool(_) => "await",
            CodeModeFunctionKind::PriorResults(_) => "no await",
        };
        text.push_str(&format!(
            "- {} ({}): {}\n  Arguments (JSON Schema): {}\n",
            definition.name,
            calling_convention,
            definition.description,
            definition.parameters.as_value(),
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{
        codemode_functions::prior_results::PriorResults,
        tools::{Noop, ToolKind},
    };
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
    fn definition_describes_essential_calling_conventions() {
        let description = CodeMode::definition().description;
        for guidance in [
            "persistent Monty sandbox",
            "print() for output",
            "await tool-backed functions",
            "PRIOR_RESULTS() without await",
        ] {
            assert!(description.contains(guidance), "missing: {guidance}");
        }
        assert!(description.split_whitespace().count() <= 40);
    }

    #[test]
    fn system_prompt_keeps_essential_guidance_compact() {
        let prompt = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        }
        .system_prompt();
        let directions = prompt.tools.unwrap();
        for guidance in [
            "not full CPython",
            "Python's filesystem/network APIs are sandboxed",
            "installed tool functions can access external resources",
            "globals(), dir(), vars(), and __import__() are unavailable",
            "print() for output; final expressions aren't displayed",
            "middle-truncated",
            "Await tool-backed functions with keyword arguments",
            "return text or raise exceptions",
            "asyncio.gather",
            "finish all awaits",
            "Call PRIOR_RESULTS(index=-1)",
            "60s interpreter execution excluding host waits",
            "660s wall",
            "64 MiB execution memory",
            "16 MiB serialized checkpoint",
            "Uncaught exceptions can preserve earlier changes",
            "checkpoint failures keep the previous checkpoint",
            "External effects are not rolled back",
        ] {
            assert!(directions.contains(guidance), "missing: {guidance}");
        }
        let words = directions.split_whitespace().count()
            + prompt
                .rules
                .iter()
                .map(|rule| rule.split_whitespace().count())
                .sum::<usize>();
        assert!(words <= 200, "codemode prompt grew to {words} words");
        assert!(!directions.contains("sleep"));
        assert!(!directions.contains("Supported stdlib imports"));
        assert!(!directions.contains("await PRIOR_RESULTS"));
    }

    #[test]
    fn rules_recommend_selective_prior_results_without_promising_truncation_recovery() {
        let prompt = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        }
        .system_prompt();
        assert_eq!(prompt.rules.len(), 2);
        let rule = &prompt.rules[0];
        for guidance in [
            "PRIOR_RESULTS(-1)",
            "slice, search, or compute a short excerpt before printing",
            "print(PRIOR_RESULTS(-1)[2000:4000])",
            "cannot recover truncated-away content",
            "retained REPL data",
            "narrower read-only query",
            "Never repeat side effects",
        ] {
            assert!(rule.contains(guidance), "missing: {guidance}");
        }
    }

    #[test]
    fn rules_recommend_native_reset_for_bad_codemode_state() {
        let prompt = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        }
        .system_prompt();
        let rule = &prompt.rules[1];
        assert!(rule.contains("If codemode gets into a bad state"));
        assert!(rule.contains("call reset_codemode directly (not inside Python)"));
        assert!(rule.contains("clears Python state, not history or external effects"));
    }

    #[test]
    fn codemode_is_scheduled_sequentially() {
        let tool = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        };
        assert_eq!(tool.execution_mode(), ExecutionMode::Sequential);
    }

    #[test]
    fn registered_function_guidance_distinguishes_sync_and_async_calls() {
        let functions = [
            CodeModeFunctionKind::PriorResults(PriorResults),
            CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
        ];
        let text = format_codemode_functions(&functions);
        assert!(text.starts_with("Available codemode functions:\n"));
        let (prior_results, noop) = text.split_once("- noop (await):").unwrap();
        assert!(prior_results.contains("- PRIOR_RESULTS (no await):"));
        assert!(!prior_results.contains("await PRIOR_RESULTS"));
        assert!(!noop.contains("no await"));
        for function in functions {
            let schema = function.definition().parameters;
            assert!(text.contains(&format!("Arguments (JSON Schema): {}", schema.as_value())));
        }
    }

    #[test]
    fn prior_results_catalogue_publishes_generated_defaulted_index_schema() {
        let function = CodeModeFunctionKind::PriorResults(PriorResults);
        let definition = function.definition();
        let schema = definition.parameters.as_value();
        assert_eq!(definition.name, "PRIOR_RESULTS");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["index"]["type"], "integer");
        assert_eq!(schema["properties"]["index"]["default"], -1);
        assert!(
            schema
                .get("required")
                .is_none_or(|required| required == &json!([]))
        );
        let text = format_codemode_functions(&[function]);
        assert!(text.contains(&format!("Arguments (JSON Schema): {schema}")));
        assert!(text.contains("- PRIOR_RESULTS (no await):"));
    }
}
