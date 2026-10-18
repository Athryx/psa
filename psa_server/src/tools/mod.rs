use llm_api::api::ToolDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolId {}

/// This is a native tool call by the model. Codemode tools are seperate.
pub trait Tool {
    fn tool_definition(&self) -> ToolDefinition;
}
