use crate::{
    agents::{
        AgentState, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        agent_projection::AgentProjection,
    },
    hooks::cancellation::{Cancellation, run_cancellable},
    prompts,
};
use actors::UnboundedMailbox;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa_api::events::ToolResult as ToolResultEvent;
use std::time::Duration;

/// Tool output content, independent of call IDs and event metadata.
pub use psa_api::{ToolId, ToolResult};

/// A model-facing tool response. `error` does not control agent execution.
pub struct ToolOutput {
    pub content: ToolResult,
    pub error: bool,
}

impl ToolOutput {
    pub fn success(content: ToolResult) -> Self {
        Self {
            content,
            error: false,
        }
    }

    pub fn error(content: ToolResult) -> Self {
        Self {
            content,
            error: true,
        }
    }
}

pub fn tool_definition(id: ToolId) -> ToolDefinition {
    match id {
        ToolId::Noop => Noop::tool_definition(),
    }
}

pub(crate) async fn execute(
    call: &api::ToolCall,
    context: &mut AgentContext<'_>,
    interrupted: &mut bool,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> ToolOutput {
    if *interrupted
        || is_incomplete_call(context.projection(), &call.call_id)
        || context.agent().state != AgentState::Running
    {
        return ToolOutput::error(prompts::tool_not_run());
    }

    let Some(id) = context
        .projection()
        .tool_ids()
        .find(|id| tool_definition(*id).name == call.name)
    else {
        return ToolOutput::error(prompts::tool_unknown());
    };

    let Ok(arguments) = serde_json::from_str(&call.arguments) else {
        return ToolOutput::error(prompts::tool_invalid_arguments());
    };

    // Keep control-message updates separate from the invocation's session snapshot.
    let mut session = context.session().clone();
    let (sender, cancellation) = Cancellation::channel();
    let (result, cancelled) = match id {
        ToolId::Noop => {
            run_cancellable(
                Noop.execute(arguments, context, cancellation),
                sender,
                Noop.cancellation_timeout(),
                &mut session,
                controls,
            )
            .await
        }
    };

    context.update_session(session);
    *interrupted |= cancelled;

    result.unwrap_or_else(|| ToolOutput::error(prompts::tool_timed_out()))
}

fn is_incomplete_call(context: &AgentProjection, call_id: &str) -> bool {
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
        _context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        if cancellation.is_cancelled() {
            return ToolOutput::error(prompts::tool_interrupted());
        }

        if !arguments
            .as_object()
            .is_some_and(|object| object.is_empty())
        {
            return ToolOutput::error(prompts::tool_invalid_arguments());
        }

        ToolOutput::success(Vec::new())
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }
}

/// A native model tool. Persist auxiliary events through the context; the worker
/// persists the returned content and error flag as a separate tool response.
pub trait Tool: Send + Sync {
    fn tool_definition() -> ToolDefinition;
    fn cancellation_timeout(&self) -> Duration;
    fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;
}
