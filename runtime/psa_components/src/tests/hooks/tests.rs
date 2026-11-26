use crate::tools::{DynTool, Edit, Read, Write, tool_definition};
use psa::api::ToolId;

#[test]
fn file_tool_instances_have_stable_ids_definitions_and_prompt_guidance() {
    let tools: [(Box<dyn DynTool>, _, _); 3] = [
        (Box::new(Read), ToolId::Read, "read"),
        (Box::new(Edit), ToolId::Edit, "edit"),
        (Box::new(Write), ToolId::Write, "write"),
    ];
    for (tool, id, name) in tools {
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
