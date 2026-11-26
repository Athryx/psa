//! Concrete internal functions callable from codemode. The function interface
//! is re-exported from `psa_agents`.

pub(crate) mod decide;
pub(crate) mod help;
mod host;
pub(crate) mod list_decision_models;
pub(crate) mod prior_results;

pub(crate) use host::AgentCodeModeHost;

pub use psa_agents::codemode_functions::*;

#[cfg(test)]
#[path = "../tests/codemode_functions/decision_tests.rs"]
mod decision_tests;
#[cfg(test)]
#[path = "../tests/codemode_functions/registration_tests.rs"]
mod registration_tests;
#[cfg(test)]
#[path = "../tests/codemode_functions/typed_tests.rs"]
mod typed_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_types_name_their_class_and_show_only_fields() {
        let function = ToolFunction::new(crate::tools::Read);
        let text = help::format_stub(function.python_stub());
        println!("{text}");
        assert!(text.contains("async def read(*, path: str"));
        assert!(text.contains("-> ReadResult:"));
        assert!(text.starts_with("class ReadResult:\n    \"\"\"A selected range"));
        assert!(text.contains("    next_offset: int | None  # Pass as offset to continue"));
        assert!(!text.contains("__str__"));
        assert!(!text.contains("_text"));

        let noop = ToolFunction::new(psa_agents::bundles::testing::test_tool(
            "noop",
            psa::api::ToolId::Edit,
        ));
        assert!(noop.python_stub().function.contains("-> str:"));
    }
}
