//! Concrete native tools. The tool interface is re-exported from `psa_agents`.

mod ask_question;
pub(crate) mod bash;
pub(crate) mod codemode;
mod edit;
mod export_trace;
mod file_ops;
pub(crate) mod goal;
mod list_sessions;
mod read;
mod record_result;
mod reset_codemode;
pub(crate) mod subagents;
mod write;

pub use ask_question::AskQuestion;
pub use bash::{Bash, CloseBash, InteractBash};
pub use codemode::CodeMode;
pub use edit::Edit;
pub use export_trace::ExportTrace;
pub use goal::{SetGoal, SetGoalStatus};
pub use list_sessions::ListSessions;
pub use read::Read;
pub use record_result::RecordResult;
pub use reset_codemode::ResetCodeMode;
pub use subagents::{CreateAgent, ListAgents, SendMessage, WaitMessage};
pub use write::Write;

#[cfg(test)]
#[path = "../tests/tools/definition_persistence_tests.rs"]
mod definition_persistence_tests;
#[cfg(test)]
#[path = "../tests/tools/tests.rs"]
mod tests;

pub use psa_agents::tools::*;

use llm_api::api::ToolDefinition;

/// Configuration-independent metadata. Dynamic tools require their installed
/// instance to supply the actual schema; no permissive placeholder is returned.
pub fn tool_definition(id: ToolId) -> Option<ToolDefinition> {
    Some(match id {
        ToolId::RecordResult => return None,
        ToolId::AskQuestion => AskQuestion::definition(),
        ToolId::CodeMode => CodeMode::definition(),
        ToolId::ResetCodeMode => ResetCodeMode::definition(),
        ToolId::Read => psa_agents::tools::Tool::tool_definition(&Read),
        ToolId::Edit => psa_agents::tools::Tool::tool_definition(&Edit),
        ToolId::Write => psa_agents::tools::Tool::tool_definition(&Write),
        ToolId::Bash => Bash::definition(),
        ToolId::InteractBash => InteractBash::definition(),
        ToolId::CloseBash => CloseBash::definition(),
        ToolId::SetGoal => SetGoal::definition(),
        ToolId::SetGoalStatus => SetGoalStatus::definition(),
        ToolId::CreateAgent => CreateAgent::definition(),
        ToolId::SendMessage => SendMessage::definition(),
        ToolId::WaitMessage => WaitMessage::definition(),
        ToolId::ListAgents => ListAgents::definition(),
        ToolId::ExportTrace => ExportTrace::definition(),
        ToolId::ListSessions => ListSessions::definition(),
    })
}
