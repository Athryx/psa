//! Publishes the codemode function catalogue as its own system-prompt section.
//! Installed by the CodeMode bundle; forks preserve their inherited prompt.
use super::{Hook, HookError};
use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        codemode_functions::{CodeModeFunction, CodeModeFunctionKind, python_stub},
    },
};
use psa_api::events::{EventType, Message, NewEvent};
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

/// Render the installed Python functions as stubs, without publishing native
/// tool definitions. `native_tools` names the agent's direct tools so shared
/// functions can be explained once instead of duplicated in prose.
pub(crate) fn format_codemode_functions(
    functions: &[CodeModeFunctionKind],
    native_tools: &[String],
) -> Option<String> {
    if functions.is_empty() {
        return None;
    }

    let mut classes = BTreeSet::new();
    let mut stubs = Vec::new();
    let mut shared = false;
    for function in functions {
        let definition = function.definition();
        let is_async = matches!(function, CodeModeFunctionKind::Tool(_));
        shared |= native_tools.contains(&definition.name);
        let stub = python_stub::render_python_stub(
            &definition.name,
            &definition.description,
            definition.parameters.as_value(),
            is_async,
        );
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
        codemode_functions::prior_results::PriorResults,
        tools::{Noop, ToolKind},
    };

    #[test]
    fn registered_functions_render_as_python_stubs_in_their_own_section() {
        let functions = [
            CodeModeFunctionKind::PriorResults(PriorResults),
            CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)),
        ];
        let text = format_codemode_functions(&functions, &[]).unwrap();
        assert!(text.starts_with("<codemode_functions>\n"));
        assert!(text.ends_with("```\n</codemode_functions>"));
        assert!(text.contains("```python\n"));
        assert!(text.contains("def PRIOR_RESULTS(*, index: int = -1) -> str:"));
        assert!(!text.contains("async def PRIOR_RESULTS"));
        assert!(text.contains("async def noop("));
        assert!(!text.contains("JSON Schema"));
        assert!(!text.contains("$schema"));
        assert!(!text.contains("direct tool"));
    }

    #[test]
    fn shared_native_tools_are_explained_once() {
        let functions = [CodeModeFunctionKind::Tool(ToolKind::Noop(Noop))];
        let text = format_codemode_functions(&functions, &["noop".into()]).unwrap();
        assert_eq!(text.matches("direct tool").count(), 1);
    }

    #[test]
    fn empty_catalogue_has_no_section() {
        assert_eq!(format_codemode_functions(&[], &[]), None);
    }
}
