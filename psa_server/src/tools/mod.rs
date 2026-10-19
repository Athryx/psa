use llm_api::api::{ContentBlock, ToolDefinition};

/// Tool output content, independent of call IDs and event metadata.
pub type ToolResult = Vec<ContentBlock>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolId {
    Noop,
}

impl ToolId {
    pub fn tool_definition(self) -> ToolDefinition {
        match self {
            Self::Noop => Noop::tool_definition(),
        }
    }
}

pub struct Noop;

impl Tool for Noop {
    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: "A placeholder tool that does nothing.".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            strict: Some(true),
        }
    }
}

/// This is a native tool call by the model. Codemode tools are seperate.
pub trait Tool {
    fn tool_definition() -> ToolDefinition;
}
