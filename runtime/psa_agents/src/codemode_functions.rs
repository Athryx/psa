//! Internal functions callable from codemode, independent of model-facing tools.
//!
//! Bundles register instances alongside their tools so both paths use the same
//! runtime-owned handles. The default adapter delegates to the tool's existing
//! typed argument bridge and preserves its cancellation behavior.

pub use psa_codemode::CodeModeError;
use psa_codemode::{
    HostFunction,
    python_stub::{self, PythonStub},
    result_classes,
};
use std::time::Duration;

use futures_util::future::BoxFuture;
use llm_api::api::{ContentBlock, ToolSchema};
use serde_json::Value;

use crate::{
    agent_context::AgentContext,
    cancellation::Cancellation,
    tools::{CodeModeResultType, DynTool, Tool, ToolOutput},
};

#[derive(Debug, Clone)]
pub struct CodeModeFunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: ToolSchema,
}

pub type CodeModeFunctionResult = Result<Value, CodeModeError>;

/// Decode arguments with the same bounded, path-aware diagnostics as native
/// tools. The shared decoder suppresses Serde messages that can echo values.
pub fn parse_codemode_arguments<T: serde::de::DeserializeOwned>(
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
#[macro_export]
macro_rules! typed_codemode_execute {
    ($arguments:ty, $execute_typed:ident) => {
        fn execute<'a>(
            &'a self,
            arguments: $crate::__private::serde_json::Value,
            context: &'a mut $crate::agent_context::AgentContext<'_>,
            mut cancellation: $crate::cancellation::Cancellation,
        ) -> $crate::__private::BoxFuture<'a, $crate::codemode_functions::CodeModeFunctionResult> {
            Box::pin(async move {
                if cancellation.is_cancelled() {
                    return Err($crate::codemode_functions::CodeModeError::runtime(
                        "Execution cancelled",
                    ));
                }
                let arguments =
                    $crate::codemode_functions::parse_codemode_arguments::<$arguments>(arguments)?;
                self.$execute_typed(arguments, context, cancellation).await
            })
        }
    };
}

pub use crate::typed_codemode_execute;

pub trait CodeModeFunction: Send + Sync + 'static {
    fn definition(&self) -> CodeModeFunctionDefinition;
    fn cancellation_timeout(&self) -> Duration;
    fn execute<'a>(
        &'a self,
        arguments: Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, CodeModeFunctionResult>;

    /// Async functions return Python futures, so scripts can run them
    /// concurrently; plain lookups return values directly.
    fn is_async(&self) -> bool {
        false
    }

    /// A field that may also be passed as the single positional argument.
    fn positional_field(&self) -> Option<&'static str> {
        None
    }

    /// The Python type shown for a function without a result class.
    fn return_annotation(&self) -> &'static str {
        "str"
    }

    /// The class this function returns instead of text, if any.
    fn result_type(&self) -> Option<CodeModeResultType> {
        None
    }

    /// The model-facing tool this function adapts, if any.
    fn tool_id(&self) -> Option<psa::api::ToolId> {
        None
    }

    /// How the CodeMode engine sees and calls this function.
    fn host_function(&self) -> HostFunction {
        let definition = self.definition();
        HostFunction {
            name: definition.name,
            description: definition.description,
            is_async: self.is_async(),
            positional_field: self.positional_field(),
            cancellation_timeout: self.cancellation_timeout(),
        }
    }

    /// The Python stub shown to the model, identical in the prompt and `help`,
    /// including the fields of its result class.
    fn python_stub(&self) -> PythonStub {
        let definition = self.definition();
        let result_type = self.result_type();
        let returns = result_type
            .as_ref()
            .map_or(self.return_annotation(), |result| &result.name);
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
    pub function: Box<dyn CodeModeFunction>,
    pub listing: CodeModeListing,
}

/// Exposes a model-facing tool to CodeMode under its own name and schema.
/// A distinct registration from the tool itself; both may share bundle state.
pub struct ToolFunction(Box<dyn DynTool>);

impl ToolFunction {
    pub fn new(tool: impl Tool + 'static) -> Self {
        Self(Box::new(tool))
    }

    pub fn from_dyn(tool: Box<dyn DynTool>) -> Self {
        Self(tool)
    }
}

impl CodeModeFunction for ToolFunction {
    fn definition(&self) -> CodeModeFunctionDefinition {
        let definition = self.0.tool_definition();
        CodeModeFunctionDefinition {
            name: definition.name,
            description: definition.description,
            parameters: definition.parameters,
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        self.0.cancellation_timeout()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, CodeModeFunctionResult> {
        Box::pin(async move {
            adapt_tool_output(self.0.execute(arguments, context, cancellation).await)
        })
    }

    fn is_async(&self) -> bool {
        true
    }

    fn result_type(&self) -> Option<CodeModeResultType> {
        self.0.codemode_result_type()
    }

    fn tool_id(&self) -> Option<psa::api::ToolId> {
        Some(self.0.id())
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
#[cfg(any(test, feature = "testing"))]
pub fn listed_entries(
    functions: impl IntoIterator<Item = Box<dyn CodeModeFunction>>,
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
mod tests {
    use super::*;
    use crate::prompts;

    #[test]
    fn adapter_uses_tool_definition_and_typed_schema() {
        let test_tool = || crate::bundles::testing::test_tool("noop", psa::api::ToolId::Edit);
        let function = ToolFunction::new(test_tool());
        let definition = function.definition();
        let tool = test_tool();
        let tool_definition = Tool::tool_definition(&tool);
        assert_eq!(definition.name, tool_definition.name);
        assert_eq!(definition.description, tool_definition.description);
        assert_eq!(definition.parameters, tool_definition.parameters);
        assert_eq!(
            function.cancellation_timeout(),
            Tool::cancellation_timeout(&tool)
        );
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
        let mut installer = crate::bundles::Installer::default();
        installer
            .add_codemode_function(ToolFunction::new(crate::bundles::testing::test_tool(
                "noop",
                psa::api::ToolId::Edit,
            )))
            .unwrap();
        assert!(
            installer
                .add_codemode_function(ToolFunction::new(crate::bundles::testing::test_tool(
                    "noop",
                    psa::api::ToolId::Edit
                )))
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
            .push(psa_store::PrivateEvent::CodeModeSnapshot(vec![1]));
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
