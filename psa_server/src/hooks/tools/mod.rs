use crate::{
    agents::{
        Agent, AgentState, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        agent_projection::AgentProjection,
    },
    hooks::cancellation::{Cancellation, run_cancellable},
    prompts,
    sessions::Session,
};
use actors::UnboundedMailbox;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa_api::events::ToolResult as ToolResultEvent;
use std::time::Duration;

/// Tool output content, independent of call IDs and event metadata.
pub use psa_api::{ToolId, ToolResult};

/// Optional human-facing tool directions and shared rules for the system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSystemPrompt {
    /// Rendered as `- <tool name>: <directions>` inside `<tools>`.
    pub tools: Option<String>,
    /// Rendered as Markdown list items inside `<rules>`.
    pub rules: Vec<String>,
}

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
        ToolId::Noop => Noop::definition(),
    }
}

pub(crate) async fn execute(
    tools: &[ToolKind],
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

    let Some(tool) = context
        .projection()
        .tool_definitions()
        .iter()
        .find(|snapshot| snapshot.definition.name == call.name)
        .and_then(|snapshot| tools.iter().find(|tool| tool.id() == snapshot.id))
    else {
        return ToolOutput::error(prompts::tool_unknown());
    };

    let Ok(arguments) = serde_json::from_str(&call.arguments) else {
        return ToolOutput::error(prompts::tool_invalid_arguments());
    };

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

pub enum ToolKind {
    Noop(Noop),
}

impl ToolKind {
    pub fn new(id: ToolId, agent: &Agent, session: &Session) -> Self {
        match id {
            ToolId::Noop => Self::Noop(Noop::new(agent, session)),
        }
    }

    pub fn id(&self) -> ToolId {
        match self {
            Self::Noop(_) => ToolId::Noop,
        }
    }

    pub fn tool_definition(&self) -> ToolDefinition {
        match self {
            Self::Noop(tool) => tool.tool_definition(),
        }
    }
}

impl Tool for ToolKind {
    fn new(agent: &Agent, session: &Session) -> Self {
        Self::new(ToolId::Noop, agent, session)
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::tool_definition(self)
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        match self {
            Self::Noop(tool) => tool.system_prompt(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Noop(tool) => tool.cancellation_timeout(),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        match self {
            Self::Noop(tool) => tool.execute(arguments, context, cancellation).await,
        }
    }
}

pub struct Noop;

impl Noop {
    fn definition() -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: prompts::noop_description(),
            parameters: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            strict: Some(true),
        }
    }
}

impl Tool for Noop {
    fn new(_agent: &Agent, _session: &Session) -> Self {
        Self
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
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
    /// Builds an agent-local instance without performing IO.
    fn new(agent: &Agent, session: &Session) -> Self
    where
        Self: Sized;

    fn tool_definition(&self) -> ToolDefinition;

    /// Additional system-prompt guidance, independent of the API tool schema.
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt::default()
    }

    fn cancellation_timeout(&self) -> Duration;
    fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;
}
