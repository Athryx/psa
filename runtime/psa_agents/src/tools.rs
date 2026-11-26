//! The native tool interface, tool output, and call preparation.

#[cfg(any(test, feature = "testing"))]
use crate::{AgentState, agent_manager::AgentManagerMessage, cancellation::run_cancellable};
use crate::{
    agent_context::AgentContext, agent_projection::AgentProjection, cancellation::Cancellation,
    prompts,
};
#[cfg(any(test, feature = "testing"))]
use actors::UnboundedMailbox;
use futures_util::future::BoxFuture;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa::api::events::ToolResult as ToolResultEvent;
use std::time::Duration;

/// Implements the untyped tool boundary for an inherent typed execution method.
///
/// The method receives deserialized arguments, the invocation context, and its
/// cancellation token. Cancellation takes precedence over argument validation.
#[macro_export]
macro_rules! typed_tool_execute {
    ($arguments:ty, $execute_typed:ident) => {
        async fn execute(
            &self,
            arguments: $crate::__private::serde_json::Value,
            context: &mut $crate::agent_context::AgentContext<'_>,
            mut cancellation: $crate::cancellation::Cancellation,
        ) -> $crate::tools::ToolOutput {
            if cancellation.is_cancelled() {
                return $crate::tools::ToolOutput::error($crate::prompts::tool_interrupted());
            }

            let arguments =
                match $crate::__private::llm_api::api::parse_tool_arguments::<$arguments>(arguments)
                {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        return $crate::tools::ToolOutput::error(
                            $crate::prompts::tool_argument_error(&error.to_string()),
                        );
                    }
                };

            self.$execute_typed(arguments, context, cancellation).await
        }
    };
}

pub use crate::typed_tool_execute;

pub use psa::api::ToolId;

/// Tool output content, independent of call IDs and event metadata.
pub type ToolResult = Vec<llm_api::api::ContentBlock>;

/// Optional human-facing tool directions and shared rules for the system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSystemPrompt {
    /// Rendered as `- <tool name>: <directions>` inside `<tools>`.
    pub tools: Option<String>,
    /// Rendered as Markdown list items inside `<rules>`.
    pub rules: Vec<String>,
}

/// The Python class a tool's CodeMode result is exposed as, declared statically
/// so stubs can name it before the tool runs. Fields come from the schema;
/// `str()` of an instance is the tool's text output.
#[derive(Debug, Clone)]
pub struct CodeModeResultType {
    pub name: String,
    pub schema: llm_api::api::ToolSchema,
}

impl CodeModeResultType {
    pub fn for_type<T: schemars::JsonSchema>() -> Self {
        Self {
            name: T::schema_name().into_owned(),
            schema: llm_api::api::ToolSchema::for_type::<T>(),
        }
    }
}

/// A model-facing tool response. `error` does not control agent execution.
pub struct ToolOutput {
    pub content: ToolResult,
    pub error: bool,
    /// Fields of the tool's CodeMode result object, matching its declared
    /// [`CodeModeResultType`]. Native tool calls ignore them. When present,
    /// CodeMode returns the object even if `error` is set.
    pub codemode_fields: Option<serde_json::Map<String, serde_json::Value>>,
    /// Private events committed atomically with this tool result.
    pub private_events: Vec<psa_store::PrivateEvent>,
    /// Agent message this call delivered; client-facing only.
    pub delivery: Option<psa::api::events::AgentMessageLink>,
}

impl ToolOutput {
    pub fn success(content: ToolResult) -> Self {
        Self {
            content,
            error: false,
            codemode_fields: None,
            private_events: Vec::new(),
            delivery: None,
        }
    }

    pub fn error(content: ToolResult) -> Self {
        Self {
            content,
            error: true,
            codemode_fields: None,
            private_events: Vec::new(),
            delivery: None,
        }
    }

    /// Attach the CodeMode result fields; `fields` must serialize as an object.
    pub fn with_codemode_fields(mut self, fields: &impl serde::Serialize) -> Self {
        match serde_json::to_value(fields) {
            Ok(serde_json::Value::Object(map)) => self.codemode_fields = Some(map),
            _ => tracing::warn!("codemode result fields did not serialize as an object"),
        }
        self
    }
}

/// Whether an invocation may overlap adjacent tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

/// Resolve the persisted tool snapshot and decode JSON before any side effects.
pub(crate) fn prepare_call(
    tools: &[Box<dyn DynTool>],
    call: &api::ToolCall,
    context: &AgentProjection,
    running: bool,
) -> Result<(usize, serde_json::Value), ToolOutput> {
    if !running || is_incomplete_call(context, &call.call_id) {
        return Err(ToolOutput::error(prompts::tool_not_run()));
    }
    let Some(index) = context
        .tool_definitions()
        .iter()
        .find(|snapshot| snapshot.definition.name == call.name)
        .and_then(|snapshot| tools.iter().position(|tool| tool.id() == snapshot.id))
    else {
        return Err(ToolOutput::error(prompts::tool_unknown()));
    };
    let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
        ToolOutput::error(prompts::tool_argument_json_error(
            error.line(),
            error.column(),
        ))
    })?;
    Ok((index, arguments))
}

/// Test seam: run one call to completion against `tools`, as the worker would.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub async fn execute(
    tools: &[Box<dyn DynTool>],
    call: &api::ToolCall,
    context: &mut AgentContext<'_>,
    interrupted: &mut bool,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> ToolOutput {
    let (index, arguments) = match prepare_call(
        tools,
        call,
        context.projection(),
        !*interrupted && context.agent().state == AgentState::Running,
    ) {
        Ok(prepared) => prepared,
        Err(output) => return output,
    };
    let tool = &tools[index];

    // Keep control-message updates separate from the invocation's session snapshot.
    let mut session = context.session().clone();
    let (sender, cancellation) = Cancellation::channel();
    let (result, cancelled) = run_cancellable(
        tool.execute(arguments, context, cancellation),
        sender,
        tool.cancellation_timeout(),
        &mut session,
        controls,
    )
    .await;

    context.update_session(session);
    *interrupted |= cancelled;

    result.unwrap_or_else(|| ToolOutput::error(prompts::tool_timed_out()))
}

fn is_incomplete_call(context: &AgentProjection, call_id: &str) -> bool {
    context.items().iter().any(|item| {
        matches!(&item.kind, HistoryItemKind::ToolCall(call) if call.call_id == call_id)
            && item
                .metadata
                .item_status
                .as_ref()
                .is_some_and(llm_api::api::ItemStatus::is_unfinished)
    })
}

pub(crate) fn failed_tool_result(call_id: String, content: ToolResult) -> ToolResultEvent {
    ToolResultEvent {
        delivery: None,
        call_id,
        content,
        error: true,
        metadata: Default::default(),
    }
}

/// An installed tool as the agent runtime stores it. Implement [`Tool`]; the
/// blanket impl provides this object-safe form.
pub trait DynTool: Send + Sync + 'static {
    fn id(&self) -> ToolId;
    fn tool_definition(&self) -> ToolDefinition;
    fn system_prompt(&self) -> ToolSystemPrompt;
    fn execution_mode(&self) -> ExecutionMode;
    fn cancellation_timeout(&self) -> Duration;
    fn codemode_result_type(&self) -> Option<CodeModeResultType>;
    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, ToolOutput>;
}

impl<T: Tool + 'static> DynTool for T {
    fn id(&self) -> ToolId {
        Tool::id(self)
    }

    fn tool_definition(&self) -> ToolDefinition {
        Tool::tool_definition(self)
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        Tool::system_prompt(self)
    }

    fn execution_mode(&self) -> ExecutionMode {
        Tool::execution_mode(self)
    }

    fn cancellation_timeout(&self) -> Duration {
        Tool::cancellation_timeout(self)
    }

    fn codemode_result_type(&self) -> Option<CodeModeResultType> {
        Tool::codemode_result_type(self)
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(Tool::execute(self, arguments, context, cancellation))
    }
}

/// Installed tools, shared between a worker and its invocation snapshots.
pub type Tools = std::sync::Arc<[Box<dyn DynTool>]>;

/// A native model tool. Persist auxiliary events through the context; the worker
/// persists the returned content and error flag as a separate tool response.
pub trait Tool: Send + Sync {
    /// The stable identity persisted with tool definition snapshots.
    fn id(&self) -> ToolId;

    fn tool_definition(&self) -> ToolDefinition;

    /// Additional system-prompt guidance, independent of the API tool schema.
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt::default()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn cancellation_timeout(&self) -> Duration;

    /// The class CodeMode returns for this tool; `None` returns its text.
    fn codemode_result_type(&self) -> Option<CodeModeResultType> {
        None
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;
}
