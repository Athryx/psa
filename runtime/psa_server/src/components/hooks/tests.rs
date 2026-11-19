use crate::components::tools::{Edit, Read, Tool, ToolKind, Write, tool_definition};
use psa::api::ToolId;

#[test]
fn file_tool_instances_have_stable_ids_definitions_and_prompt_guidance() {
    for (tool, id, name) in [
        (ToolKind::Read(Read), ToolId::Read, "read"),
        (ToolKind::Edit(Edit), ToolId::Edit, "edit"),
        (ToolKind::Write(Write), ToolId::Write, "write"),
    ] {
        assert_eq!(tool.id(), id);
        let definition = tool.tool_definition();
        assert_eq!(definition.name, name);
        assert_eq!(definition.strict, Some(false));
        assert!(
            definition
                .parameters
                .validate_openai_parameters(definition.strict)
                .is_ok()
        );
        assert_eq!(
            serde_json::to_value(&definition).unwrap(),
            serde_json::to_value(tool_definition(id).unwrap()).unwrap()
        );
        assert!(tool.system_prompt().tools.is_some());
        assert!(!tool.system_prompt().rules.is_empty());
    }
}
