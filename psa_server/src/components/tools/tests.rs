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
