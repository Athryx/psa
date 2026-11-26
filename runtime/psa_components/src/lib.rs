//! The built-in agent components: bundles and the tools, hooks, and codemode
//! functions they install. The interfaces they implement and the runtime that
//! runs them live in `psa_agents`; [`bundles::BuiltinComponents`] is the
//! factory the server hands the runtime.

use psa::api::events::ToolDefinition;

pub mod bundles;
pub mod codemode_functions;
pub mod hooks;
pub mod prompts;
pub mod tools;
pub mod trace_export;

/// Test and client convenience for taking a tool definition snapshot.
pub trait ToolDefinitionExt: Sized {
    fn for_tool(id: crate::tools::ToolId) -> Option<Self>;
}

impl ToolDefinitionExt for ToolDefinition {
    fn for_tool(id: crate::tools::ToolId) -> Option<Self> {
        Some(Self {
            id,
            definition: crate::tools::tool_definition(id)?,
        })
    }
}

/// Store fixtures shared by unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    #[allow(unused_imports)]
    pub use psa_store::test_support::{
        create_agent_definition, model_config, probe_metadata, register_probe,
    };
}
