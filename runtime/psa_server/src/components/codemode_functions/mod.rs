//! Internal functions callable from codemode, independent of model-facing tools.
//!
//! Bundles register instances alongside their tools so both paths use the same
//! runtime-owned handles. The default adapter delegates to the tool's existing
//! typed argument bridge and preserves its cancellation behavior.

use std::time::Duration;

use futures_util::future::BoxFuture;
use llm_api::api::{ContentBlock, ToolSchema};
use serde_json::Value;

use crate::{
    agents::agent_context::AgentContext,
    components::{
        cancellation::Cancellation,
        tools::{Tool, ToolKind, ToolOutput},
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

/// A distinct registration from a model-facing tool; both may share bundle state.
pub enum CodeModeFunctionKind {
    Tool(ToolKind),
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
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Tool(tool) => tool.cancellation_timeout(),
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
            }
        })
    }
}

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
    if output.error {
        Err(CodeModeError::runtime(text))
    } else {
        Ok(Value::String(text))
    }
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
