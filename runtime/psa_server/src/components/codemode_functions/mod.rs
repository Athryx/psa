//! Internal functions callable from codemode, independent of model-facing tools.
//!
//! Bundles register instances alongside their tools so both paths use the same
//! runtime-owned handles. The default adapter delegates to the tool's existing
//! typed argument bridge and preserves its cancellation behavior.

pub(crate) mod help;
pub(crate) mod prior_results;
pub(crate) mod python_stub;
pub(crate) mod result_classes;

use help::Help;
use prior_results::PriorResults;
use python_stub::PythonStub;
use std::time::Duration;

use futures_util::future::BoxFuture;
use llm_api::api::{ContentBlock, ToolSchema};
use serde_json::Value;

use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        tools::{CodeModeResultType, Tool, ToolKind, ToolOutput},
    },
};

#[derive(Debug, Clone)]
pub struct CodeModeFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: ToolSchema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeModeError {
    pub exception_type: String,
    pub message: String,
}

impl CodeModeError {
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            exception_type: "RuntimeError".into(),
            message: message.into(),
        }
    }
}

pub type CodeModeFunctionResult = Result<Value, CodeModeError>;

/// Decode arguments with the same bounded, path-aware diagnostics as native
/// tools. The shared decoder suppresses Serde messages that can echo values.
#[allow(dead_code, reason = "available to custom codemode functions")]
pub(crate) fn parse_codemode_arguments<T: serde::de::DeserializeOwned>(
    arguments: Value,
) -> Result<T, CodeModeError> {
    llm_api::api::parse_tool_arguments(arguments).map_err(|error| {
        let message = crate::prompts::tool_argument_error(&error.to_string())
            .into_iter()
            .find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text),
                _ => None,
            })
            .unwrap_or_else(|| "Invalid tool arguments".into());
        CodeModeError {
            exception_type: "TypeError".into(),
            message,
        }
    })
}

/// Generate the JSON-to-typed bridge for a custom codemode function. The named
/// async method takes the typed arguments, context and cancellation token.
/// Cancellation takes precedence over decoding, as for typed native tools.
#[allow(unused_macros, reason = "available to custom codemode functions")]
macro_rules! typed_codemode_execute {
    ($arguments:ty, $execute_typed:ident) => {
        fn execute<'a>(
            &'a self,
            arguments: serde_json::Value,
            context: &'a mut crate::agents::agent_context::AgentContext<'_>,
            mut cancellation: crate::components::cancellation::Cancellation,
        ) -> futures_util::future::BoxFuture<
            'a,
            crate::components::codemode_functions::CodeModeFunctionResult,
        > {
            Box::pin(async move {
                if cancellation.is_cancelled() {
                    return Err(
                        crate::components::codemode_functions::CodeModeError::runtime(
                            "Execution cancelled",
                        ),
                    );
                }
                let arguments = crate::components::codemode_functions::parse_codemode_arguments::<
                    $arguments,
                >(arguments)?;
                self.$execute_typed(arguments, context, cancellation).await
            })
        }
    };
}

#[allow(unused_imports, reason = "available to custom codemode functions")]
pub(crate) use typed_codemode_execute;

pub trait CodeModeFunction: Send + Sync {
    fn definition(&self) -> CodeModeFunctionDefinition;
    fn cancellation_timeout(&self) -> Duration;
    fn execute<'a>(
        &'a self,
        arguments: Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, CodeModeFunctionResult>;
}

/// Where the model learns a function's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeModeListing {
    /// Rendered as a stub in the system prompt's codemode function section.
    SystemPrompt,
    /// Named in the system prompt; its stub is returned only by `help`.
    HelpOnly,
}

/// An installed function together with how it is documented to the model.
pub struct CodeModeEntry {
    pub function: CodeModeFunctionKind,
    pub listing: CodeModeListing,
}

/// A distinct registration from a model-facing tool; both may share bundle state.
pub enum CodeModeFunctionKind {
    Tool(ToolKind),
    PriorResults(PriorResults),
    Help(Help),
}

impl CodeModeFunctionKind {
    /// Tool adapters return Python futures; built-in lookups return plain values.
    pub fn is_async(&self) -> bool {
        matches!(self, Self::Tool(_))
    }

    /// The class this function returns instead of text, if any.
    pub fn result_type(&self) -> Option<CodeModeResultType> {
        match self {
            Self::Tool(tool) => tool.codemode_result_type(),
            Self::PriorResults(_) | Self::Help(_) => None,
        }
    }

    /// The Python stub shown to the model, identical in the prompt and `help`,
    /// including the fields of its result class.
    pub(crate) fn python_stub(&self) -> PythonStub {
        let definition = self.definition();
        let result_type = self.result_type();
        let returns = result_type.as_ref().map_or("str", |result| &result.name);
        let mut stub = python_stub::render_python_stub(
            &definition.name,
            &definition.description,
            definition.parameters.as_value(),
            self.is_async(),
            returns,
        );
        if let Some(result) = &result_type {
            stub.classes.extend(python_stub::render_result_class(
                &result.name,
                result.schema.as_value(),
            ));
        }
        stub
    }
}

impl CodeModeFunction for CodeModeFunctionKind {
    fn definition(&self) -> CodeModeFunctionDefinition {
        match self {
            Self::Tool(tool) => {
                let definition = tool.tool_definition();
                CodeModeFunctionDefinition {
                    name: definition.name,
                    description: definition.description,
                    parameters: definition.parameters,
                }
            }
            Self::PriorResults(function) => function.definition(),
            Self::Help(function) => function.definition(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Tool(tool) => tool.cancellation_timeout(),
            Self::PriorResults(function) => function.cancellation_timeout(),
            Self::Help(function) => function.cancellation_timeout(),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, CodeModeFunctionResult> {
        Box::pin(async move {
            match self {
                Self::Tool(tool) => {
                    adapt_tool_output(tool.execute(arguments, context, cancellation).await)
                }
                Self::PriorResults(function) => {
                    function.execute(arguments, context, cancellation).await
                }
                Self::Help(function) => function.execute(arguments, context, cancellation).await,
            }
        })
    }
}

/// Text for plain tools. A tool with result fields yields an object holding the
/// fields plus its text, even when it flagged an error: the fields describe
/// the outcome, and the prelude class wraps the object.
fn adapt_tool_output(output: ToolOutput) -> CodeModeFunctionResult {
    if !output.private_events.is_empty() {
        return Err(CodeModeError::runtime(
            "The default codemode adapter cannot persist this tool's private events; its side effects may already have occurred",
        ));
    }

    let mut parts = Vec::with_capacity(output.content.len());
    for block in output.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text),
            _ => {
                return Err(CodeModeError::runtime(
                    "Tool returned non-text content that codemode cannot represent",
                ));
            }
        }
    }
    let text = parts.join("\n");
    if let Some(mut fields) = output.codemode_fields {
        fields.insert(result_classes::TEXT_FIELD.into(), Value::String(text));
        return Ok(Value::Object(fields));
    }
    if output.error {
        Err(CodeModeError::runtime(text))
    } else {
        Ok(Value::String(text))
    }
}

/// Test fixture: a registry with every function listed in the system prompt.
#[cfg(test)]
pub(crate) fn listed_entries(
    functions: impl IntoIterator<Item = CodeModeFunctionKind>,
) -> std::sync::Arc<[CodeModeEntry]> {
    functions
        .into_iter()
        .map(|function| CodeModeEntry {
            function,
            listing: CodeModeListing::SystemPrompt,
        })
        .collect()
}

#[cfg(test)]
mod registration_tests;
#[cfg(test)]
mod typed_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        components::tools::{Noop, ToolKind},
        prompts,
    };

    #[test]
    fn adapter_uses_tool_definition_and_typed_schema() {
        let function = CodeModeFunctionKind::Tool(ToolKind::Noop(Noop));
        let definition = function.definition();
        let tool = Noop.tool_definition();
        assert_eq!(definition.name, tool.name);
        assert_eq!(definition.description, tool.description);
        assert_eq!(definition.parameters, tool.parameters);
        assert_eq!(function.cancellation_timeout(), Noop.cancellation_timeout());
    }

    #[test]
    fn result_types_name_their_class_and_show_only_fields() {
        let function = CodeModeFunctionKind::Tool(ToolKind::Read(crate::components::tools::Read));
        let text = help::format_stub(function.python_stub());
        println!("{text}");
        assert!(text.contains("async def read(*, path: str"));
        assert!(text.contains("-> ReadResult:"));
        assert!(text.starts_with("class ReadResult:\n    \"\"\"A selected range"));
        assert!(text.contains("    next_offset: int | None  # Pass as offset to continue"));
        assert!(!text.contains("__str__"));
        assert!(!text.contains("_text"));

        let noop = CodeModeFunctionKind::Tool(ToolKind::Noop(Noop));
        assert!(noop.python_stub().function.contains("-> str:"));
    }

    #[test]
    fn structured_outputs_keep_fields_and_text_even_when_flagged_as_errors() {
        let output = ToolOutput::error(prompts::text("failed"))
            .with_codemode_fields(&serde_json::json!({"exit_code": 3}));
        assert_eq!(
            adapt_tool_output(output).unwrap(),
            serde_json::json!({"exit_code": 3, "_text": "failed"})
        );
    }

    #[test]
    fn duplicate_function_names_are_rejected() {
        let mut installer = crate::components::bundles::Installer::default();
        installer
            .add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)))
            .unwrap();
        assert!(
            installer
                .add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::Noop(Noop)))
                .is_err()
        );
    }

    #[test]
    fn non_text_content_is_rejected_even_when_mixed_with_text() {
        use llm_api::api::{ImageBlock, MediaSource};
        let mut blocks = prompts::text("visible");
        blocks.push(ContentBlock::Image(ImageBlock {
            source: MediaSource::Inline {
                data: vec![1, 2],
                mime_type: "image/png".into(),
            },
            detail: None,
        }));
        let error = adapt_tool_output(ToolOutput::success(blocks)).unwrap_err();
        assert_eq!(error.exception_type, "RuntimeError");
        assert!(error.message.contains("non-text"));
    }

    #[test]
    fn adapter_rejects_private_events_instead_of_discarding_them() {
        let mut output = ToolOutput::success(prompts::text("visible"));
        output
            .private_events
            .push(crate::db::PrivateEvent::CodeModeSnapshot(vec![1]));
        let error = adapt_tool_output(output).unwrap_err();
        assert_eq!(error.exception_type, "RuntimeError");
        assert!(error.message.contains("cannot persist"));
        assert!(
            error
                .message
                .contains("side effects may already have occurred")
        );
    }

    #[test]
    fn adapter_converts_success_and_errors_without_losing_text() {
        assert_eq!(
            adapt_tool_output(ToolOutput::success(prompts::text("hello"))).unwrap(),
            Value::String("hello".into())
        );
        assert_eq!(
            adapt_tool_output(ToolOutput::error(prompts::text("bad"))).unwrap_err(),
            CodeModeError::runtime("bad")
        );
        let mut blocks = prompts::text("one");
        blocks.extend(prompts::text("two"));
        assert_eq!(
            adapt_tool_output(ToolOutput::success(blocks)).unwrap(),
            Value::String("one\ntwo".into())
        );
    }
}
