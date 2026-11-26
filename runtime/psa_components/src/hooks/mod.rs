//! Concrete hooks. The hook interface is re-exported from `psa_agents`.

pub mod codemode_functions;
pub mod compaction;
pub mod cwd;
pub mod default_startup;
pub mod extra_system_prompt;
pub mod goal;
pub mod project_context;
pub mod require_result;
pub mod session_model_update;
pub mod skills;
pub mod stop_without_tools;
pub(crate) mod subagent_completion;
pub mod subagent_identity;
pub mod tool_definitions;

#[cfg(test)]
#[path = "../tests/hooks/tests.rs"]
mod tests;

pub use codemode_functions::CodeModeFunctionsHook;
pub use cwd::CwdHook;
pub use default_startup::DefaultStartupHook;
pub use extra_system_prompt::ExtraSystemPromptHook;
pub use goal::GoalHook;
pub use project_context::ProjectContextHook;
pub use require_result::RequireResult;
pub use session_model_update::SessionModelUpdateHook;
pub use skills::SkillsHook;
pub use stop_without_tools::StopWithoutTools;
pub use subagent_identity::SubagentIdentity;
pub use tool_definitions::ToolDefinitionsHook;

pub use psa_agents::hooks::*;
