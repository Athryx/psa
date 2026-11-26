use super::{DynTool, Edit, Read, ToolId, Write, tool_definition};
use llm_api::api::{ToolDefinition, parse_tool_arguments};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NestedArgs {
    entries: Vec<NestedEntry>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NestedEntry {
    count: u32,
}

#[test]
fn concrete_unit_tools_have_their_registered_identities() {
    let tools: [(Box<dyn DynTool>, _); 3] = [
        (Box::new(Read), ToolId::Read),
        (Box::new(Edit), ToolId::Edit),
        (Box::new(Write), ToolId::Write),
    ];
    for (tool, id) in tools {
        assert_eq!(tool.id(), id);
        assert_eq!(
            tool.tool_definition().name,
            tool_definition(id).unwrap().name
        );
    }
}

#[test]
fn every_tool_identity_has_metadata_without_runtime_resources() {
    for (id, name) in [
        (ToolId::AskQuestion, "ask_question"),
        (ToolId::ResetCodeMode, "reset_codemode"),
        (ToolId::Read, "read"),
        (ToolId::Edit, "edit"),
        (ToolId::Write, "write"),
        (ToolId::Bash, "bash"),
        (ToolId::InteractBash, "interact_bash"),
        (ToolId::CloseBash, "close_bash"),
        (ToolId::SetGoal, "set_goal"),
        (ToolId::SetGoalStatus, "set_goal_status"),
        (ToolId::CreateAgent, "create_agent"),
        (ToolId::SendMessage, "send_message"),
        (ToolId::WaitMessage, "wait_message"),
        (ToolId::ListAgents, "list_agents"),
    ] {
        assert_eq!(tool_definition(id).unwrap().name, name);
    }
}

#[test]
fn configured_result_identity_has_no_schema_without_its_bundle() {
    assert!(tool_definition(ToolId::RecordResult).is_none());
}

#[test]
fn typed_arguments_parse_nested_values() {
    let arguments = parse_tool_arguments::<NestedArgs>(json!({
        "entries": [{"count": 7}]
    }))
    .unwrap();

    assert_eq!(arguments.entries[0].count, 7);
}

#[test]
fn typed_arguments_report_nested_field_paths_without_echoing_values() {
    let error = parse_tool_arguments::<NestedArgs>(json!({
        "entries": [{"count": "private-argument-value"}]
    }))
    .unwrap_err()
    .to_string();

    assert!(error.contains("entries[0].count"), "{error}");
    assert!(!error.contains("private-argument-value"), "{error}");
}

#[test]
fn typed_arguments_reject_missing_and_unknown_fields() {
    for arguments in [
        json!({}),
        json!({"entries": [{}]}),
        json!({"entries": [], "extra": true}),
        json!({"entries": [{"count": 1, "extra": true}]}),
    ] {
        assert!(parse_tool_arguments::<NestedArgs>(arguments).is_err());
    }
}

#[test]
fn typed_tool_definitions_preserve_explicit_non_strict_mode() {
    let definition =
        ToolDefinition::for_type::<NestedArgs>("nested", "Nested arguments", Some(false));
    let parameters = serde_json::to_value(&definition.parameters).unwrap();

    assert_eq!(definition.name, "nested");
    assert_eq!(definition.description, "Nested arguments");
    assert_eq!(definition.strict, Some(false));
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(parameters["required"], json!(["entries"]));
}
