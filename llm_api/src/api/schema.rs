//! Shared locally knowable OpenAI-compatible function-tool validation.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::ToolDefinition;
use crate::Error;

/// Both adapters use the same function-name grammar and schema shape checks.
/// Both raw and generated schemas receive the same conservative strict checks.
/// Provider-specific capabilities may still be rejected by the server.
pub(super) fn validate_tool(tool: &ToolDefinition) -> Result<(), Error> {
    let validate = || -> Result<(), String> {
        if tool.name.is_empty()
            || tool.name.len() > 64
            || !tool
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("function names must be 1-64 ASCII letters, digits, '_' or '-'".into());
        }

        tool.parameters
            .to_openai_parameters(tool.strict)
            .map(|_| ())
    };

    validate().map_err(|message| Error::InvalidToolDefinition {
        name: tool.name.clone(),
        message,
    })
}

/// Check locally knowable schema errors without inventing a restricted dialect.
/// References, combinators and provider-specific keywords are passed through;
/// the selected provider/model validates dialect and strict-mode restrictions.
pub(super) fn validate_schema(schema: &Value) -> Result<(), String> {
    if schema.is_boolean() {
        return Ok(());
    }

    let object = schema
        .as_object()
        .ok_or("a subschema must be an object or boolean")?;

    validate_type(object)?;
    validate_subschemas(object)?;
    validate_keyword_values(object)
}

fn validate_type(object: &Map<String, Value>) -> Result<(), String> {
    if let Some(kind) = object.get("type") {
        let valid = |value: &Value| {
            matches!(
                value.as_str(),
                Some("null" | "boolean" | "object" | "array" | "number" | "integer" | "string")
            )
        };
        match kind {
            Value::String(_) if valid(kind) => {}
            Value::Array(kinds) if !kinds.is_empty() && kinds.iter().all(valid) => {
                let mut seen = HashSet::new();
                if !kinds.iter().all(|kind| seen.insert(kind.as_str().unwrap())) {
                    return Err("schema type array contains duplicates".into());
                }
            }
            _ => {
                return Err(
                    "schema type must name a JSON Schema type or nonempty array of types".into(),
                );
            }
        }
    }

    Ok(())
}

fn validate_subschemas(object: &Map<String, Value>) -> Result<(), String> {
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(value) = object.get(keyword) {
            let properties = value
                .as_object()
                .ok_or_else(|| format!("schema {keyword} must be an object"))?;
            for value in properties.values() {
                validate_schema(value)?;
            }
        }
    }

    for keyword in [
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "contains",
        "additionalItems",
        "unevaluatedItems",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(value) = object.get(keyword) {
            validate_schema(value)?;
        }
    }

    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(value) = object.get(keyword) {
            let values = value
                .as_array()
                .ok_or_else(|| format!("schema {keyword} must be an array"))?;

            if values.is_empty() && keyword != "prefixItems" {
                return Err(format!("schema {keyword} must be nonempty"));
            }

            for value in values {
                validate_schema(value)?;
            }
        }
    }

    if let Some(items) = object.get("items") {
        // Tuple-form items belongs to older dialects and is still supported by
        // compatible servers. Leave dialect selection to the server.
        if let Some(items) = items.as_array() {
            for item in items {
                validate_schema(item)?;
            }
        } else {
            validate_schema(items)?;
        }
    }

    Ok(())
}

fn validate_keyword_values(object: &Map<String, Value>) -> Result<(), String> {
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or("schema required must be an array")?;
        let mut seen = HashSet::new();

        if !required
            .iter()
            .all(|name| name.as_str().is_some_and(|name| seen.insert(name)))
        {
            return Err("schema required must contain unique strings".into());
        }
    }

    for keyword in [
        "$ref",
        "$schema",
        "$id",
        "title",
        "description",
        "pattern",
        "format",
    ] {
        if object.get(keyword).is_some_and(|value| !value.is_string()) {
            return Err(format!("schema {keyword} must be a string"));
        }
    }

    for keyword in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "minContains",
        "maxContains",
    ] {
        if object
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            return Err(format!("schema {keyword} must be a nonnegative integer"));
        }
    }

    if object.get("enum").is_some_and(|value| !value.is_array()) {
        return Err("schema enum must be an array".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, parameters: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: parameters.into(),
            strict: None,
        }
    }

    #[test]
    fn validates_function_names() {
        for name in ["", "two words", "dot.name", "non-ascii-é", &"a".repeat(65)] {
            assert!(matches!(
                validate_tool(&tool(name, json!({"type":"object"}))),
                Err(Error::InvalidToolDefinition { .. })
            ));
        }

        for name in ["a", "tool_name-123", &"a".repeat(64)] {
            validate_tool(&tool(name, json!({"type":"object"}))).unwrap();
        }
    }

    #[test]
    fn rejects_known_invalid_schema_shapes() {
        for schema in [
            json!(true),
            json!(null),
            json!({"type":42}),
            json!({"type":["string","string"]}),
            json!({"type":"not-a-type"}),
            json!({"properties":[]}),
            json!({"properties":{"x":{"type":42}}}),
            json!({"required":["x","x"]}),
            json!({"required":[1]}),
            json!({"additionalProperties":3}),
            json!({"anyOf":[]}),
            json!({"items":4}),
            json!({"minLength":-1}),
            json!({"enum":"bad"}),
        ] {
            assert!(
                matches!(
                    validate_tool(&tool("test", schema.clone())),
                    Err(Error::InvalidToolDefinition { .. })
                ),
                "accepted invalid schema {schema}"
            );
        }
    }

    #[test]
    fn preserves_dialects_references_combinators_and_boolean_subschemas() {
        for schema in [
            json!({"$ref":"#/$defs/input","$defs":{"input":{"type":"object"}}}),
            json!({"allOf":[{"type":"object"},{"properties":{"x":true},"additionalProperties":false}]}),
            json!({"type":"object","properties":{"items":{"type":"array","items":{"type":["string","null"]}}},"required":["items"],"x-provider-rule":"keep"}),
        ] {
            validate_tool(&tool("test", schema)).unwrap();
        }
    }
}
