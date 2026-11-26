//! The built-in bundles. The bundle interface is re-exported from `psa_agents`.

mod ask_question;
pub mod bash;
mod builtin;
pub mod codemode;
mod compaction;
mod completion;
pub(crate) mod decision_models;
mod extra_system_prompt;
mod file_tools;
pub mod goal;
#[cfg(test)]
#[path = "../tests/bundles/lifecycle_tests.rs"]
mod lifecycle_tests;
pub mod session;
mod startup;
mod stop_without_tools;
pub mod subagents;
#[cfg(test)]
#[path = "../tests/bundles/tests.rs"]
mod tests;
mod traces;
pub(crate) mod typed_result;

pub use ask_question::AskQuestionBundle;
pub use bash::BashBundle;
pub use builtin::{
    BuiltinComponents, build_bundle, install_bundles, require_command_support, validate_bundles,
};
pub use codemode::CodeModeBundle;
pub use compaction::CompactionBundle;
pub use completion::{CompletionConfigError, validate_completion_config};
pub use decision_models::DecisionModelsBundle;
pub use extra_system_prompt::ExtraSystemPromptBundle;
pub use file_tools::FileToolsBundle;
pub use goal::GoalBundle;
pub use psa::api::BundleConfig;
pub use startup::StartupBundle;
pub use stop_without_tools::StopWithoutToolsBundle;
pub use subagents::SubagentsBundle;
pub use traces::TracesBundle;
pub use typed_result::TypedResultBundle;

pub use psa_agents::bundles::*;
