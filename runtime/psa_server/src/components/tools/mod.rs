#[cfg(test)]
use crate::{
    agents::{AgentState, agent_manager::AgentManagerMessage},
    components::cancellation::run_cancellable,
};
use crate::{
    agents::{agent_context::AgentContext, agent_projection::AgentProjection},
    components::cancellation::Cancellation,
    prompts,
};
#[cfg(test)]
use actors::UnboundedMailbox;
use llm_api::api::{self, HistoryItemKind, ToolDefinition};
use psa_api::events::ToolResult as ToolResultEvent;
use std::time::Duration;

mod ask_question;
pub(crate) mod bash;
pub(crate) mod codemode;
mod edit;
mod file_ops;
pub(crate) mod goal;
mod noop;
mod read;
mod record_result;
mod reset_codemode;
pub(crate) mod subagents;
mod write;

pub use ask_question::AskQuestion;
pub use bash::{Bash, CloseBash, InteractBash};
pub use codemode::CodeMode;
pub use edit::Edit;
pub use goal::{SetGoal, SetGoalStatus};
pub use noop::Noop;
pub use read::Read;
pub use record_result::RecordResult;
pub use reset_codemode::ResetCodeMode;
pub use subagents::{CreateAgent, ListAgents, SendMessage, WaitMessage};
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
    /// Private events committed atomically with this tool result.
    pub(crate) private_events: Vec<crate::db::PrivateEvent>,
}

impl ToolOutput {
    pub fn success(content: ToolResult) -> Self {
        Self {
            content,
            error: false,
            private_events: Vec::new(),
        }
    }

    pub fn error(content: ToolResult) -> Self {
        Self {
            content,
            error: true,
            private_events: Vec::new(),
        }
    }
}

/// Configuration-independent metadata. Dynamic tools require their installed
/// instance to supply the actual schema; no permissive placeholder is returned.
pub fn tool_definition(id: ToolId) -> Option<ToolDefinition> {
    Some(match id {
        ToolId::RecordResult => return None,
        ToolId::AskQuestion => AskQuestion::definition(),
        ToolId::Noop => Noop::definition(),
        ToolId::CodeMode => CodeMode::definition(),
        ToolId::ResetCodeMode => ResetCodeMode::definition(),
        ToolId::Read => Read.tool_definition(),
        ToolId::Edit => Edit.tool_definition(),
        ToolId::Write => Write.tool_definition(),
        ToolId::Bash => Bash::definition(),
        ToolId::InteractBash => InteractBash::definition(),
        ToolId::CloseBash => CloseBash::definition(),
        ToolId::SetGoal => SetGoal::definition(),
        ToolId::SetGoalStatus => SetGoalStatus::definition(),
        ToolId::CreateAgent => CreateAgent::definition(),
        ToolId::SendMessage => SendMessage::definition(),
        ToolId::WaitMessage => WaitMessage::definition(),
        ToolId::ListAgents => ListAgents::definition(),
    })
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
    tools: &[ToolKind],
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

#[cfg(test)]
pub(crate) async fn execute(
    tools: &[ToolKind],
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
    AskQuestion(AskQuestion),
    RecordResult(RecordResult),
    Noop(Noop),
    CodeMode(CodeMode),
    ResetCodeMode(ResetCodeMode),
    Read(Read),
    Edit(Edit),
    Write(Write),
    Bash(Bash),
    InteractBash(InteractBash),
    CloseBash(CloseBash),
    SetGoal(SetGoal),
    SetGoalStatus(SetGoalStatus),
    CreateAgent(CreateAgent),
    SendMessage(SendMessage),
    WaitMessage(WaitMessage),
    ListAgents(ListAgents),
    #[cfg(test)]
    BundleProbe(crate::components::bundles::testing::SharedTool),
    #[cfg(test)]
    ExecutionProbe(crate::agents::agent_manager::tool_execution::tests::ExecutionProbe),
}

impl ToolKind {
    pub fn id(&self) -> ToolId {
        match self {
            Self::AskQuestion(_) => ToolId::AskQuestion,
            Self::RecordResult(_) => ToolId::RecordResult,
            Self::Noop(_) => ToolId::Noop,
            Self::CodeMode(_) => ToolId::CodeMode,
            Self::ResetCodeMode(_) => ToolId::ResetCodeMode,
            Self::Read(_) => ToolId::Read,
            Self::Edit(_) => ToolId::Edit,
            Self::Write(_) => ToolId::Write,
            Self::Bash(_) => ToolId::Bash,
            Self::InteractBash(_) => ToolId::InteractBash,
            Self::CloseBash(_) => ToolId::CloseBash,
            Self::SetGoal(_) => ToolId::SetGoal,
            Self::SetGoalStatus(_) => ToolId::SetGoalStatus,
            Self::CreateAgent(_) => ToolId::CreateAgent,
            Self::SendMessage(_) => ToolId::SendMessage,
            Self::WaitMessage(_) => ToolId::WaitMessage,
            Self::ListAgents(_) => ToolId::ListAgents,
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.id,
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.id,
        }
    }

    pub fn tool_definition(&self) -> ToolDefinition {
        match self {
            Self::AskQuestion(tool) => tool.tool_definition(),
            Self::RecordResult(tool) => tool.tool_definition(),
            Self::Noop(tool) => tool.tool_definition(),
            Self::CodeMode(tool) => tool.tool_definition(),
            Self::ResetCodeMode(tool) => tool.tool_definition(),
            Self::Read(tool) => tool.tool_definition(),
            Self::Edit(tool) => tool.tool_definition(),
            Self::Write(tool) => tool.tool_definition(),
            Self::Bash(tool) => tool.tool_definition(),
            Self::InteractBash(tool) => tool.tool_definition(),
            Self::CloseBash(tool) => tool.tool_definition(),
            Self::SetGoal(tool) => tool.tool_definition(),
            Self::SetGoalStatus(tool) => tool.tool_definition(),
            Self::CreateAgent(tool) => tool.tool_definition(),
            Self::SendMessage(tool) => tool.tool_definition(),
            Self::WaitMessage(tool) => tool.tool_definition(),
            Self::ListAgents(tool) => tool.tool_definition(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.tool_definition(),
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.tool_definition(),
        }
    }
}

impl Tool for ToolKind {
    fn tool_definition(&self) -> ToolDefinition {
        Self::tool_definition(self)
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        match self {
            Self::AskQuestion(tool) => tool.system_prompt(),
            Self::RecordResult(tool) => tool.system_prompt(),
            Self::Noop(tool) => tool.system_prompt(),
            Self::CodeMode(tool) => tool.system_prompt(),
            Self::ResetCodeMode(tool) => tool.system_prompt(),
            Self::Read(tool) => tool.system_prompt(),
            Self::Edit(tool) => tool.system_prompt(),
            Self::Write(tool) => tool.system_prompt(),
            Self::Bash(tool) => tool.system_prompt(),
            Self::InteractBash(tool) => tool.system_prompt(),
            Self::CloseBash(tool) => tool.system_prompt(),
            Self::SetGoal(tool) => tool.system_prompt(),
            Self::SetGoalStatus(tool) => tool.system_prompt(),
            Self::CreateAgent(tool) => tool.system_prompt(),
            Self::SendMessage(tool) => tool.system_prompt(),
            Self::WaitMessage(tool) => tool.system_prompt(),
            Self::ListAgents(tool) => tool.system_prompt(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.system_prompt(),
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.system_prompt(),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        match self {
            Self::AskQuestion(tool) => tool.execution_mode(),
            Self::RecordResult(tool) => tool.execution_mode(),
            Self::Noop(tool) => tool.execution_mode(),
            Self::CodeMode(tool) => tool.execution_mode(),
            Self::ResetCodeMode(tool) => tool.execution_mode(),
            Self::Read(tool) => tool.execution_mode(),
            Self::Edit(tool) => tool.execution_mode(),
            Self::Write(tool) => tool.execution_mode(),
            Self::Bash(tool) => tool.execution_mode(),
            Self::InteractBash(tool) => tool.execution_mode(),
            Self::CloseBash(tool) => tool.execution_mode(),
            Self::SetGoal(tool) => tool.execution_mode(),
            Self::SetGoalStatus(tool) => tool.execution_mode(),
            Self::CreateAgent(tool) => tool.execution_mode(),
            Self::SendMessage(tool) => tool.execution_mode(),
            Self::WaitMessage(tool) => tool.execution_mode(),
            Self::ListAgents(tool) => tool.execution_mode(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.execution_mode(),
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.execution_mode(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        match self {
            Self::AskQuestion(tool) => tool.cancellation_timeout(),
            Self::RecordResult(tool) => tool.cancellation_timeout(),
            Self::Noop(tool) => tool.cancellation_timeout(),
            Self::CodeMode(tool) => tool.cancellation_timeout(),
            Self::ResetCodeMode(tool) => tool.cancellation_timeout(),
            Self::Read(tool) => tool.cancellation_timeout(),
            Self::Edit(tool) => tool.cancellation_timeout(),
            Self::Write(tool) => tool.cancellation_timeout(),
            Self::Bash(tool) => tool.cancellation_timeout(),
            Self::InteractBash(tool) => tool.cancellation_timeout(),
            Self::CloseBash(tool) => tool.cancellation_timeout(),
            Self::SetGoal(tool) => tool.cancellation_timeout(),
            Self::SetGoalStatus(tool) => tool.cancellation_timeout(),
            Self::CreateAgent(tool) => tool.cancellation_timeout(),
            Self::SendMessage(tool) => tool.cancellation_timeout(),
            Self::WaitMessage(tool) => tool.cancellation_timeout(),
            Self::ListAgents(tool) => tool.cancellation_timeout(),
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.cancellation_timeout(),
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.cancellation_timeout(),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        match self {
            Self::AskQuestion(tool) => tool.execute(arguments, context, cancellation).await,
            Self::RecordResult(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Noop(tool) => tool.execute(arguments, context, cancellation).await,
            Self::CodeMode(tool) => tool.execute(arguments, context, cancellation).await,
            Self::ResetCodeMode(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Read(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Edit(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Write(tool) => tool.execute(arguments, context, cancellation).await,
            Self::Bash(tool) => tool.execute(arguments, context, cancellation).await,
            Self::InteractBash(tool) => tool.execute(arguments, context, cancellation).await,
            Self::CloseBash(tool) => tool.execute(arguments, context, cancellation).await,
            Self::SetGoal(tool) => tool.execute(arguments, context, cancellation).await,
            Self::SetGoalStatus(tool) => tool.execute(arguments, context, cancellation).await,
            Self::CreateAgent(tool) => tool.execute(arguments, context, cancellation).await,
            Self::SendMessage(tool) => tool.execute(arguments, context, cancellation).await,
            Self::WaitMessage(tool) => tool.execute(arguments, context, cancellation).await,
            Self::ListAgents(tool) => tool.execute(arguments, context, cancellation).await,
            #[cfg(test)]
            Self::BundleProbe(tool) => tool.execute(arguments, context, cancellation).await,
            #[cfg(test)]
            Self::ExecutionProbe(tool) => tool.execute(arguments, context, cancellation).await,
        }
    }
}

/// A native model tool. Persist auxiliary events through the context; the worker
/// persists the returned content and error flag as a separate tool response.
pub trait Tool: Send + Sync {
    fn tool_definition(&self) -> ToolDefinition;

    /// Additional system-prompt guidance, independent of the API tool schema.
    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt::default()
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn cancellation_timeout(&self) -> Duration;
    fn execute(
        &self,
        arguments: serde_json::Value,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> impl std::future::Future<Output = ToolOutput> + Send;
}
