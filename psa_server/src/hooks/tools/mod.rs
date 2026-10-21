use crate::{
    agents::{AgentState, agent_context::AgentContext, agent_manager::AgentManagerMessage},
    hooks::cancellation::{Cancellation, run_cancellable},
    prompts,
    sessions::Session,
};
use actors::UnboundedMailbox;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa_api::events::ToolResult as ToolResultEvent;
use std::{path::PathBuf, time::Duration};

/// Tool output content, independent of call IDs and event metadata.
pub use psa_api::{ToolId, ToolResult};

pub fn tool_definition(id: ToolId) -> ToolDefinition {
    match id {
        ToolId::Noop => Noop::tool_definition(),
    }
}

pub struct ToolContext {
    pub working_directory: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid tool arguments")]
    InvalidArguments,
    #[error("tool was cancelled")]
    Cancelled,
}

pub(crate) async fn execute(
    call: api::ToolCall,
    context: &AgentContext,
    session: &mut Session,
    interrupted: &mut bool,
    controls: Option<&mut UnboundedMailbox<AgentManagerMessage>>,
) -> ToolResultEvent {
    if *interrupted
        || is_incomplete_call(context, &call.call_id)
        || context.status != AgentState::Running
    {
        return failed_tool_result(call.call_id, prompts::tool_not_run());
    }
    if !context
        .tool_ids()
        .any(|id| tool_definition(id).name == call.name)
    {
        return failed_tool_result(call.call_id, prompts::tool_unknown());
    }
    let Ok(arguments) = serde_json::from_str(&call.arguments) else {
        return failed_tool_result(call.call_id, prompts::tool_invalid_arguments());
    };

    let (sender, cancellation) = Cancellation::channel();
    let context = ToolContext {
        working_directory: session.working_directory.clone(),
    };
    let future = Noop.execute(arguments, context, cancellation);
    let result = match controls {
        Some(controls) => {
            let (result, cancelled) = run_cancellable(
                future,
                sender,
                Noop.cancellation_timeout(),
                session,
                controls,
            )
            .await;
            *interrupted |= cancelled;
            result
        }
        None => Some(future.await),
    };

    match result {
        Some(Ok(content)) => ToolResultEvent {
            call_id: call.call_id,
            content,
            error: false,
            metadata: Default::default(),
        },
        Some(Err(ToolError::InvalidArguments)) => {
            failed_tool_result(call.call_id, prompts::tool_invalid_arguments())
        }
        Some(Err(ToolError::Cancelled)) => {
            failed_tool_result(call.call_id, prompts::tool_interrupted())
        }
        None => failed_tool_result(call.call_id, prompts::tool_timed_out()),
    }
}

fn is_incomplete_call(context: &AgentContext, call_id: &str) -> bool {
    context.items().iter().any(|item| {
        matches!(&item.kind, HistoryItemKind::ToolCall(call) if call.call_id == call_id)
            && matches!(
                item.metadata.item_status.as_deref(),
                Some("in_progress" | "incomplete")
            )
    })
}

pub(crate) fn failed_tool_result(call_id: String, content: ToolResult) -> ToolResultEvent {
    ToolResultEvent {
        call_id,
        content,
        error: true,
        metadata: Default::default(),
    }
}

pub struct Noop;

impl Tool for Noop {
    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: prompts::noop_description(),
            parameters: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            strict: Some(true),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _context: ToolContext,
        mut cancellation: Cancellation,
    ) -> Result<ToolResult, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if !arguments
            .as_object()
            .is_some_and(|object| object.is_empty())
        {
            return Err(ToolError::InvalidArguments);
        }
        Ok(Vec::new())
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }
}

/// This is a native tool call by the model. Codemode tools are seperate.
pub trait Tool: Send + Sync {
    fn tool_definition() -> ToolDefinition;
    fn cancellation_timeout(&self) -> Duration;
    fn execute(
        &self,
        arguments: serde_json::Value,
        context: ToolContext,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = Result<ToolResult, ToolError>> + Send;
}
