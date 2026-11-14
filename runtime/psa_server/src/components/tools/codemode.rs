use super::{ExecutionMode, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};
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
            "Execute Python in a persistent sandboxed REPL. Use print() for output, await async functions, and call prior_results() without await.",
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
        if let Some(saved) = result.saved_text {
            output
                .private_events
                .push(PrivateEvent::CodeModeOutput(saved));
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
                "Runs a restricted Python subset (not full CPython) in a persistent REPL: variables, functions, and imports persist between calls. ",
                "Python's own filesystem and network access is disabled; use the functions in <codemode_functions> for external work. ",
                "globals(), dir(), vars(), and __import__() are unavailable.\n\n",
                "Use print() for output; final expressions aren't displayed. Output over 12,000 characters is middle-truncated in what you see, but prior_results returns it in full. ",
                "Call those functions with keyword arguments, awaiting the async ones; they return their stub's type (result objects print as the tool's text) and raise RuntimeError on failure. ",
                "Use asyncio.gather for independent calls, and finish all awaits before the code ends.\n\n",
                "Limits per call: 60s of Python execution (time spent inside functions doesn't count), 660s total, 64 MiB memory, and 16 MiB of saved REPL state. ",
                "If the code raises, REPL changes made before the error are kept; after a timeout, cancellation, or limit error the REPL state is unchanged. ",
                "Effects of functions, such as file writes or commands, are never rolled back."
            ).into()),
            rules: vec![concat!(
                "When codemode output is truncated, read the rest with prior_results instead of rerunning: slice, search, or compute a short excerpt before printing, ",
                "e.g. print(prior_results(-1)[6000:18000]). Never repeat side effects just to recover output."
            ).into(), "If the codemode REPL gets into a bad state, call the reset_codemode tool (it is not a Python function). It clears Python variables only, not history or external effects.".into()],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::codemode_functions::{
        CodeModeFunction, CodeModeFunctionKind, prior_results::PriorResults,
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
            "persistent sandboxed REPL",
            "print() for output",
            "await async functions",
            "prior_results() without await",
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
            "persistent REPL",
            "filesystem and network access is disabled",
            "use the functions in <codemode_functions> for external work",
            "globals(), dir(), vars(), and __import__() are unavailable",
            "print() for output; final expressions aren't displayed",
            "middle-truncated in what you see",
            "prior_results returns it in full",
            "keyword arguments, awaiting the async ones",
            "print as the tool's text) and raise RuntimeError",
            "asyncio.gather",
            "finish all awaits",
            "60s of Python execution",
            "660s total",
            "64 MiB memory",
            "16 MiB of saved REPL state",
            "REPL changes made before the error are kept",
            "the REPL state is unchanged",
            "never rolled back",
        ] {
            assert!(directions.contains(guidance), "missing: {guidance}");
        }
        let words = directions.split_whitespace().count()
            + prompt
                .rules
                .iter()
                .map(|rule| rule.split_whitespace().count())
                .sum::<usize>();
        assert!(words <= 240, "codemode prompt grew to {words} words");
        assert!(!directions.contains("sleep"));
        assert!(!directions.contains("Supported stdlib imports"));
        assert!(!directions.contains("await prior_results"));
        assert!(!directions.contains("Monty"));
        assert!(!directions.contains("checkpoint"));
    }

    #[test]
    fn rules_recommend_prior_results_for_truncated_output() {
        let prompt = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        }
        .system_prompt();
        assert_eq!(prompt.rules.len(), 2);
        let rule = &prompt.rules[0];
        for guidance in [
            "When codemode output is truncated, read the rest with prior_results",
            "slice, search, or compute a short excerpt before printing",
            "print(prior_results(-1)[6000:18000])",
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
        assert!(rule.contains("If the codemode REPL gets into a bad state"));
        assert!(rule.contains("call the reset_codemode tool (it is not a Python function)"));
        assert!(rule.contains("clears Python variables only, not history or external effects"));
    }

    #[test]
    fn codemode_is_scheduled_sequentially() {
        let tool = CodeMode {
            engine: Arc::new(MontyEngine::new("monty")),
        };
        assert_eq!(tool.execution_mode(), ExecutionMode::Sequential);
    }

    #[test]
    fn prior_results_schema_keeps_defaulted_index() {
        let function = CodeModeFunctionKind::PriorResults(PriorResults);
        let definition = function.definition();
        let schema = definition.parameters.as_value();
        assert_eq!(definition.name, "prior_results");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["index"]["type"], "integer");
        assert_eq!(schema["properties"]["index"]["default"], -1);
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
        assert!(
            schema
                .get("required")
                .is_none_or(|required| required == &json!([]))
        );
    }
}
