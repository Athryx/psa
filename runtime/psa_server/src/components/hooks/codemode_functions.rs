//! Publishes the codemode function catalogue as its own system-prompt section.
//! Installed by the CodeMode bundle; forks preserve their inherited prompt.
use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        codemode_functions::{CodeModeEntry, CodeModeFunction, CodeModeListing},
    },
};
use psa_api::events::{EventType, Message};
use std::{collections::BTreeSet, time::Duration};

pub struct CodeModeFunctionsHook;

impl Hook for CodeModeFunctionsHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if context.agent().parent_id.is_some() {
            return Ok(());
        }

        let native_tools: Vec<String> = context
            .tools()
            .iter()
            .map(|tool| tool.tool_definition().name)
            .collect();
        let Some(section) = format_codemode_functions(context.codemode_functions(), &native_tools)
        else {
            return Ok(());
        };

        let events = [NewEvent::new(EventType::Message(Message::system(
            section,
            "Codemode functions",
        )))];
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = context.save_events(&events) => { result?; },
        }
        Ok(())
    }
}

/// Render the prompt-listed Python functions as stubs, without publishing native
/// tool definitions. Help-only functions are named so the model can ask `help`.
/// `native_tools` names the agent's direct tools so shared functions can be
/// explained once instead of duplicated in prose.
pub(crate) fn format_codemode_functions(
    functions: &[CodeModeEntry],
    native_tools: &[String],
) -> Option<String> {
    if functions.is_empty() {
        return None;
    }

    let mut classes = BTreeSet::new();
    let mut stubs = Vec::new();
    let mut help_only = Vec::new();
    let mut shared = false;
    for entry in functions {
        if entry.listing == CodeModeListing::HelpOnly {
            help_only.push(entry.function.definition().name);
            continue;
        }
        shared |= native_tools.contains(&entry.function.definition().name);
        let stub = entry.function.python_stub();
        classes.extend(stub.classes);
        stubs.push(stub.function);
    }

    let mut text = String::from(
        "<codemode_functions>\nFunctions available inside the codemode tool's Python REPL:\n```python\n",
    );
    for class in classes.into_iter().chain(stubs) {
        text.push_str(&class);
        text.push_str("\n\n");
    }
    text.truncate(text.trim_end().len());
    text.push_str("\n```");
    if !help_only.is_empty() {
        text.push_str(&format!(
            "\nMore functions, documented only through help(name): {}.",
            help_only.join(", ")
        ));
    }
    if shared {
        text.push_str("\nFunctions sharing a name with a direct tool behave like that tool. Call the tool directly for a single operation; use codemode to combine calls, loop, or filter output.");
    }
    text.push_str("\n</codemode_functions>");
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{
        codemode_functions::{CodeModeFunctionKind, help::Help, prior_results::PriorResults},
        tools::{Noop, ToolKind},
    };

    fn entry(function: CodeModeFunctionKind, listing: CodeModeListing) -> CodeModeEntry {
        CodeModeEntry { function, listing }
    }

    #[test]
    fn registered_functions_render_as_python_stubs_in_their_own_section() {
        let functions = [
            entry(
                CodeModeFunctionKind::PriorResults(PriorResults),
                CodeModeListing::SystemPrompt,
            ),
            entry(
                CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
                CodeModeListing::SystemPrompt,
            ),
        ];
        let text = format_codemode_functions(&functions, &[]).unwrap();
        assert!(text.starts_with("<codemode_functions>\n"));
        assert!(text.ends_with("```\n</codemode_functions>"));
        assert!(text.contains("```python\n"));
        assert!(text.contains("def prior_results(*, index: int = -1) -> str:"));
        assert!(!text.contains("async def prior_results"));
        assert!(text.contains("async def noop("));
        assert!(!text.contains("JSON Schema"));
        assert!(!text.contains("$schema"));
        assert!(!text.contains("direct tool"));
        assert!(!text.contains("help(name)"));
    }

    #[test]
    fn help_only_functions_are_named_without_stubs() {
        let functions = [
            entry(
                CodeModeFunctionKind::Help(Help),
                CodeModeListing::SystemPrompt,
            ),
            entry(
                CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
                CodeModeListing::HelpOnly,
            ),
        ];
        let text = format_codemode_functions(&functions, &[]).unwrap();
        assert!(text.contains("def help(*, name: str | None = None) -> str:"));
        assert!(!text.contains("async def noop("));
        assert!(text.contains("documented only through help(name): noop."));
    }

    #[test]
    fn shared_native_tools_are_explained_once() {
        let functions = [entry(
            CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
            CodeModeListing::SystemPrompt,
        )];
        let text = format_codemode_functions(&functions, &["noop".into()]).unwrap();
        assert_eq!(text.matches("direct tool").count(), 1);
    }

    #[test]
    fn empty_catalogue_has_no_section() {
        assert_eq!(format_codemode_functions(&[], &[]), None);
    }
}
