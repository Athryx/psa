//! `help`: returns the stub of any installed codemode function, including those
//! kept out of the system prompt.

use std::time::Duration;

use llm_api::api::ToolSchema;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{
    CodeModeEntry, CodeModeError, CodeModeFunction, CodeModeFunctionDefinition,
    CodeModeFunctionResult, python_stub::PythonStub, typed_codemode_execute,
};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};

pub struct Help;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HelpArgs {
    /// Function to describe. Omit to list every function with a one-line summary.
    #[serde(default)]
    pub(super) name: Option<String>,
}

impl Help {
    async fn execute_typed(
        &self,
        arguments: HelpArgs,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> CodeModeFunctionResult {
        let entries = context.codemode_functions();
        let Some(name) = arguments.name else {
            return Ok(Value::String(format_function_list(entries)));
        };

        let entry = entries
            .iter()
            .find(|entry| entry.function.definition().name == name)
            .ok_or_else(|| CodeModeError {
                exception_type: "NameError".into(),
                message: format!("No codemode function named '{name}'. Call help() to list them."),
            })?;
        Ok(Value::String(format_stub(entry.function.python_stub())))
    }
}

impl CodeModeFunction for Help {
    fn definition(&self) -> CodeModeFunctionDefinition {
        CodeModeFunctionDefinition {
            name: "help".into(),
            description: "Return the Python stub and documentation of a codemode function, or list all functions when name is omitted. Not async; do not await.".into(),
            parameters: ToolSchema::for_type::<HelpArgs>(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_codemode_execute!(HelpArgs, execute_typed);
}

/// Referenced `TypedDict` classes first, as in the system prompt section.
pub(crate) fn format_stub(stub: PythonStub) -> String {
    let mut parts: Vec<String> = stub.classes.into_iter().collect();
    parts.push(stub.function);
    parts.join("\n\n")
}

fn format_function_list(entries: &[CodeModeEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let definition = entry.function.definition();
            let summary = definition.description.lines().next().unwrap_or_default();
            format!("{}: {summary}", definition.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::codemode_functions::{CodeModeFunctionKind, CodeModeListing};

    #[test]
    fn stub_matches_prompt_rendering_and_list_summarizes_each_function() {
        let entries = [
            CodeModeEntry {
                function: CodeModeFunctionKind::Help(Help),
                listing: CodeModeListing::SystemPrompt,
            },
            CodeModeEntry {
                function: CodeModeFunctionKind::Tool(
                    crate::components::bundles::testing::test_tool("noop", psa::api::ToolId::Edit),
                ),
                listing: CodeModeListing::HelpOnly,
            },
        ];
        let stub = format_stub(entries[1].function.python_stub());
        assert!(stub.starts_with("async def noop("));

        let list = format_function_list(&entries);
        assert_eq!(list.lines().count(), 2);
        assert!(list.starts_with("help: "));
        assert!(list.contains("\nnoop: "));
    }
}
