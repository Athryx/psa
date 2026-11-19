use crate::components::tools::{Edit, Noop, Read, Tool, ToolKind, Write, tool_definition};
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

#[test]
fn tool_instances_supply_the_fixture_definition_and_identity() {
    let tool = ToolKind::Noop(Noop);
    let native = Noop;

    assert_eq!(tool.id(), ToolId::Noop);
    let expected = serde_json::to_value(tool_definition(ToolId::Noop).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(tool.tool_definition()).unwrap(),
        expected
    );
    assert_eq!(
        serde_json::to_value(native.tool_definition()).unwrap(),
        expected
    );
    assert_eq!(native.system_prompt().tools, None);
    assert!(native.system_prompt().rules.is_empty());
    assert_eq!(tool.system_prompt(), native.system_prompt());
}
