use crate::{
    agents::{
        Agent, AgentState, agent_context::AgentContext, agent_manager::AgentManagerMessage,
        agent_projection::AgentProjection,
    },
    components::cancellation::{Cancellation, run_cancellable},
    prompts,
    sessions::Session,
};
use actors::UnboundedMailbox;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa_api::events::ToolResult as ToolResultEvent;
use std::time::Duration;

pub(crate) mod bash;
mod edit;
mod file_ops;
mod noop;
mod read;
mod write;

pub use bash::{Bash, CloseBash, InteractBash};
pub use edit::Edit;
pub use noop::Noop;
pub use read::Read;
pub use write::Write;

#[cfg(test)]
mod tests;

/// Implements the untyped tool boundary for an inherent typed execution method.
///
/// The method receives deserialized arguments, the invocation context, and its
/// cancellation token. Cancellation takes precedence over argument validation.
macro_rules! typed_tool_execute {
    ($arguments:ty, $execute_typed:ident) => {
        async fn execute(
            &self,
            arguments: serde_json::Value,
            context: &mut crate::agents::agent_context::AgentContext<'_>,
            mut cancellation: crate::components::cancellation::Cancellation,
        ) -> crate::components::tools::ToolOutput {
            if cancellation.is_cancelled() {
                return crate::components::tools::ToolOutput::error(
                    crate::prompts::tool_interrupted(),
                );
            }

            let arguments = match llm_api::api::parse_tool_arguments::<$arguments>(arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return crate::components::tools::ToolOutput::error(
                        crate::prompts::tool_argument_error(&error.to_string()),
                    );
                }
            };

            self.$execute_typed(arguments, context, cancellation).await
        }
    };
}

pub(crate) use typed_tool_execute;

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
        ToolId::Read => Read.tool_definition(),
        ToolId::Edit => Edit.tool_definition(),
        ToolId::Write => Write.tool_definition(),
        ToolId::Bash => Bash::default().tool_definition(),
        ToolId::InteractBash => InteractBash::default().tool_definition(),
        ToolId::CloseBash => CloseBash::default().tool_definition(),
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

    let arguments = match serde_json::from_str(&call.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            return ToolOutput::error(prompts::tool_argument_json_error(
                error.line(),
                error.column(),
            ));
        }
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
    Read(Read),
    Edit(Edit),
    Write(Write),
    Bash(Bash),
    InteractBash(InteractBash),
    CloseBash(CloseBash),
    #[cfg(test)]
    BundleProbe(crate::components::bundles::testing::SharedTool),
}

impl ToolKind {
    pub fn required_bundle(id: ToolId) -> Option<psa_api::BundleId> {
        match id {
            ToolId::Bash | ToolId::InteractBash | ToolId::CloseBash => {
                Some(psa_api::BundleId::Bash)
            }
            _ => None,
        }
    }

    pub fn new(id: ToolId, agent: &Agent, session: &Session) -> Self {
        match id {
            ToolId::Noop => Self::Noop(Noop::new(agent, session)),
            ToolId::Read => Self::Read(Read::new(agent, session)),
            ToolId::Edit => Self::Edit(Edit::new(agent, session)),
            ToolId::Write => Self::Write(Write::new(agent, session)),
            ToolId::Bash => Self::Bash(Bash::new(agent, session)),
            ToolId::InteractBash => Self::InteractBash(InteractBash::new(agent, session)),
            ToolId::CloseBash => Self::CloseBash(CloseBash::new(agent, session)),
        }
    }

    pub fn id(&self) -> ToolId {
        match self {
            Self::Noop(_) => ToolId::Noop,
            Self::Read(_) => ToolId::Read,
            Self::Edit(_) => ToolId::Edit,
            Self::Write(_) => ToolId::Write,
            Self::Bash(_) => ToolId::Bash,
            Self::InteractBash(_) => ToolId::InteractBash,
            Self::CloseBash(_) => ToolId::CloseBash,
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.id,
        }
    }

    pub fn tool_definition(&self) -> ToolDefinition {
        match self {
            Self::Noop(tool) => tool.tool_definition(),
            Self::Read(tool) => tool.tool_definition(),
            Self::Edit(tool) => tool.tool_definition(),
            Self::Write(tool) => tool.tool_definition(),
            Self::Bash(tool) => tool.tool_definition(),
            Self::InteractBash(tool) => tool.tool_definition(),
            Self::CloseBash(tool) => tool.tool_definition(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.tool_definition(),
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
            Self::Read(tool) => tool.system_prompt(),
            Self::Edit(tool) => tool.system_prompt(),
            Self::Write(tool) => tool.system_prompt(),
            Self::Bash(tool) => tool.system_prompt(),
            Self::InteractBash(tool) => tool.system_prompt(),
            Self::CloseBash(tool) => tool.system_prompt(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.system_prompt(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::Noop(tool) => tool.cancellation_timeout(),
            Self::Read(tool) => tool.cancellation_timeout(),
            Self::Edit(tool) => tool.cancellation_timeout(),
            Self::Write(tool) => tool.cancellation_timeout(),
            Self::Bash(tool) => tool.cancellation_timeout(),
            Self::InteractBash(tool) => tool.cancellation_timeout(),
            Self::CloseBash(tool) => tool.cancellation_timeout(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.cancellation_timeout(),
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
            Self::Read(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Edit(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Write(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Bash(tool) => tool.execute(arguments, context, cancellation).await,
            Self::InteractBash(tool) => tool.execute(arguments, context, cancellation).await,
            Self::CloseBash(tool) => tool.execute(arguments, context, cancellation).await,
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.execute(arguments, context, cancellation).await,
        }
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
