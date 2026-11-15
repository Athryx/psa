//! Portable JSON Schema and typed tool argument decoding.

use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// An owned JSON Schema, serialized exactly as the schema object in snapshots.
///
/// Keeping JSON preserves dialect-specific keywords, references, and annotations
/// without an incomplete second schema AST. `From<Value>` is the raw-schema escape
/// hatch. Raw and generated schemas receive identical validation, including
/// after snapshot round trips; strict mode supports a conservative common subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ToolSchema {
    schema: Value,
}

impl ToolSchema {
    /// Generate the schema for inputs accepted by Serde, not serialized outputs.
    ///
    /// ```
    /// use llm_api::api::ToolDefinition;
    /// use schemars::JsonSchema;
    /// #[derive(serde::Deserialize, JsonSchema)]
    /// struct Arguments {
    ///     /// A value that may be omitted (not made required-and-nullable).
    ///     value: Option<String>,
    /// }
    /// let tool = ToolDefinition::for_type::<Arguments>("example", "Example tool", Some(false));
    /// assert_eq!(tool.strict, Some(false));
    /// ```
    pub fn for_type<T: JsonSchema>() -> Self {
        let schema = SchemaSettings::draft2020_12()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<T>();
        let mut schema = serde_json::to_value(schema).expect("JSON Schema serializes as JSON");
        if let Some(root) = schema.as_object_mut() {
            root.remove("$schema");
            root.remove("title");
        }
        remove_generated_noise(&mut schema);
        Self { schema }
    }

    pub fn as_value(&self) -> &Value {
        &self.schema
    }

    /// Check the schema can be sent unchanged to either OpenAI-compatible protocol.
    ///
    /// Strict generation requires closed objects with every property required.
    /// Optional/defaulted fields cannot be silently made required or nullable:
    /// absence and explicit null have different Serde semantics. Use non-strict
    /// mode, or explicitly design strict-compatible argument types. This checks a
    /// conservative common subset, not model-specific capabilities. Arbitrary
    /// dialect keywords are preserved in non-strict mode; strict mode rejects
    /// unsupported keywords rather than silently removing them.
    pub fn validate_openai_parameters(&self, strict: Option<bool>) -> Result<(), String> {
        if !self.schema.is_object() {
            return Err("parameters must be a JSON Schema object".into());
        }
        super::schema::validate_schema(&self.schema)?;
        if strict == Some(true) {
            if self.schema.get("type").and_then(Value::as_str) != Some("object") {
                return Err("strict tool parameters must have an object root".into());
            }
            validate_strict_schema(&self.schema)?;
        }
        Ok(())
    }
}

impl From<Value> for ToolSchema {
    fn from(schema: Value) -> Self {
        Self { schema }
    }
}

/// Drop generator annotations that add tokens without guiding the model: Rust
/// numeric storage formats and `default: null` on omittable fields.
fn remove_generated_noise(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            let is_numeric_format =
                object
                    .get("format")
                    .and_then(Value::as_str)
                    .is_some_and(|format| {
                        matches!(
                            format,
                            "double"
                                | "float"
                                | "int8"
                                | "int16"
                                | "int32"
                                | "int64"
                                | "uint"
                                | "uint8"
                                | "uint16"
                                | "uint32"
                                | "uint64"
                        )
                    });
            if is_numeric_format {
                object.remove("format");
            }
            if object.get("default") == Some(&Value::Null) {
                object.remove("default");
            }
            object.values_mut().for_each(remove_generated_noise);
        }
        Value::Array(items) => items.iter_mut().for_each(remove_generated_noise),
        _ => {}
    }
}

fn validate_strict_schema(schema: &Value) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or("strict schemas must be objects, not boolean schemas")?;
    for keyword in object.keys() {
        if !matches!(
            keyword.as_str(),
            "$schema"
                | "$defs"
                | "$ref"
                | "title"
                | "description"
                | "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "anyOf"
                | "enum"
        ) {
            return Err(format!("unsupported strict schema keyword: {keyword}"));
        }
    }
    let has_type = |kind: &str| {
        object.get("type").is_some_and(|value| {
            value.as_str() == Some(kind)
                || value
                    .as_array()
                    .is_some_and(|types| types.iter().any(|t| t.as_str() == Some(kind)))
        })
    };
    if has_type("array") && !object.contains_key("items") {
        return Err("strict arrays require an items schema".into());
    }
    if has_type("object") || object.contains_key("properties") {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err("strict objects require additionalProperties: false (use serde deny_unknown_fields)".into());
        }
        let properties = object.get("properties").and_then(Value::as_object);
        let required = object.get("required").and_then(Value::as_array);
        let property_count = properties.map_or(0, |p| p.len());
        if required.map_or(0, |r| r.len()) != property_count
            || properties.is_some_and(|p| {
                p.keys().any(|name| {
                    !required.is_some_and(|r| r.iter().any(|v| v.as_str() == Some(name)))
                })
            })
        {
            return Err("strict objects require every property; optional/defaulted fields cannot be made required without changing semantics".into());
        }
    }
    for keyword in ["properties", "$defs"] {
        if let Some(children) = object.get(keyword).and_then(Value::as_object) {
            for child in children.values() {
                validate_strict_schema(child)?;
            }
        }
    }
    if let Some(items) = object.get("items") {
        validate_strict_schema(items)?;
    }
    if let Some(variants) = object.get("anyOf").and_then(Value::as_array) {
        for variant in variants {
            validate_strict_schema(variant)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::api::ToolDefinition;
    use serde_json::json;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Nested {
        /// The displayed label.
        label: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Arguments {
        /// Entries to submit.
        #[serde(rename(deserialize = "entries", serialize = "output_entries"))]
        children: Vec<Nested>,
        optional: Option<String>,
        #[serde(default)]
        count: u32,
        #[serde(skip_deserializing)]
        output_only: bool,
    }

    #[test]
    fn generates_deserialize_contract_with_refs_docs_defaults_and_optionality() {
        let schema = ToolSchema::for_type::<Arguments>();
        let value = schema.as_value();
        assert_eq!(
            value["properties"]["entries"]["description"],
            "Entries to submit."
        );
        assert!(value["properties"].get("output_entries").is_none());
        assert!(value["properties"].get("output_only").is_none());
        assert_eq!(
            value["properties"]["entries"]["items"]["$ref"],
            "#/$defs/Nested"
        );
        assert_eq!(
            value["$defs"]["Nested"]["properties"]["label"]["description"],
            "The displayed label."
        );
        assert_eq!(value["required"], json!(["entries"]));
        assert_eq!(value["properties"]["count"]["default"], 0);
        assert!(value["properties"]["optional"].to_string().contains("null"));
        let tool = ToolDefinition::for_type::<Arguments>("submit", "Submit entries", Some(false));
        assert_eq!(tool.strict, Some(false));
        schema.validate_openai_parameters(Some(false)).unwrap();
        let args: Arguments = parse_tool_arguments(json!({"entries":[{"label":"ok"}]})).unwrap();
        assert_eq!(args.children[0].label, "ok");
        assert_eq!(args.optional, None);
        assert_eq!(args.count, 0);
        assert!(!args.output_only);
    }

    #[test]
    fn defaulted_optional_fields_preserve_absent_versus_null_semantics() {
        fn default_value() -> Option<String> {
            Some("default".into())
        }
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        struct Input {
            #[serde(default = "default_value")]
            value: Option<String>,
        }
        let schema = ToolSchema::for_type::<Input>();
        let before = schema.as_value().clone();
        assert!(schema.validate_openai_parameters(Some(true)).is_err());
        schema.validate_openai_parameters(Some(false)).unwrap();
        assert_eq!(schema.as_value(), &before);
        assert_eq!(
            parse_tool_arguments::<Input>(json!({}))
                .unwrap()
                .value
                .as_deref(),
            Some("default")
        );
        assert_eq!(
            parse_tool_arguments::<Input>(json!({"value":null}))
                .unwrap()
                .value,
            None
        );
    }

    #[test]
    fn schema_and_tool_snapshots_keep_the_original_json_shape() {
        let tool = ToolDefinition::for_type::<Arguments>("submit", "Submit entries", Some(false));
        let snapshot = serde_json::to_value(&tool).unwrap();
        assert_eq!(snapshot["parameters"], *tool.parameters.as_value());
        let restored: ToolDefinition = serde_json::from_value(snapshot.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), snapshot);
        let raw = json!({"$ref":"#/$defs/Input", "$defs":{"Input":{"type":"object"}}, "x-extension": true});
        let schema: ToolSchema = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(schema).unwrap(), raw);
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct StrictArguments {
        child: Nested,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct OptionalArguments {
        maybe: Option<String>,
    }

    #[derive(JsonSchema)]
    struct OpenArguments {
        value: String,
    }

    #[test]
    fn tool_definitions_preserve_requested_strict_policy() {
        for strict in [None, Some(false), Some(true)] {
            let definition =
                ToolDefinition::for_type::<StrictArguments>("submit", "Submit entries", strict);
            assert_eq!(definition.strict, strict);
            let snapshot = serde_json::to_value(&definition).unwrap();
            match strict {
                Some(value) => assert_eq!(snapshot["strict"], value),
                None => assert!(snapshot.get("strict").is_none()),
            }
            assert!(
                definition
                    .parameters
                    .validate_openai_parameters(strict)
                    .is_ok()
            );
        }
    }

    #[test]
    fn strict_validation_rejects_incompatibilities_without_mutation() {
        for schema in [
            ToolSchema::for_type::<Arguments>(),
            ToolSchema::for_type::<OptionalArguments>(),
            ToolSchema::for_type::<OpenArguments>(),
            ToolSchema::for_type::<String>(),
            ToolSchema::from(
                json!({"type":"object","properties":{"nested":{"type":["object","null"]}},"required":["nested"],"additionalProperties":false}),
            ),
            ToolSchema::from(
                json!({"type":"object","properties":{"list":{"type":"array"}},"required":["list"],"additionalProperties":false}),
            ),
        ] {
            let before = schema.as_value().clone();
            assert!(schema.validate_openai_parameters(Some(true)).is_err());
            schema.validate_openai_parameters(Some(false)).unwrap();
            assert_eq!(*schema.as_value(), before);
            let restored: ToolSchema = serde_json::from_value(before).unwrap();
            assert!(restored.validate_openai_parameters(Some(true)).is_err());
        }
        let schema = ToolSchema::for_type::<StrictArguments>();
        schema.validate_openai_parameters(Some(true)).unwrap();
        // Raw schemas retain full-dialect pass-through in non-strict mode.
        let raw = json!({"type":"object", "properties":{"optional":{"type":"string"}}, "x-provider":"keep"});
        ToolSchema::from(raw)
            .validate_openai_parameters(Some(false))
            .unwrap();
        let unsupported = ToolSchema::from(
            json!({"type":"object","properties":{},"additionalProperties":false,"allOf":[{}]}),
        );
        assert!(
            unsupported
                .validate_openai_parameters(Some(true))
                .unwrap_err()
                .contains("allOf")
        );
    }

    #[test]
    fn argument_errors_include_array_paths_but_not_secret_values() {
        #[derive(Debug, Deserialize)]
        struct Empty {}
        for value in [json!([]), json!(null), json!(42), json!("secret")] {
            assert!(
                parse_tool_arguments::<Empty>(value)
                    .unwrap_err()
                    .message
                    .contains("JSON object")
            );
        }
        let error = parse_tool_arguments::<Arguments>(
            json!({"entries":[{"label":"ok"},{"label":{"secret":"DO_NOT_ECHO"}}]}),
        )
        .unwrap_err();
        assert_eq!(error.path, "entries[1].label");
        assert!(error.message.contains("invalid type"));
        assert!(!error.to_string().contains("DO_NOT_ECHO"));
        let scalar =
            parse_tool_arguments::<Arguments>(json!({"entries":[], "count":"SCALAR_SECRET"}))
                .unwrap_err();
        assert_eq!(scalar.path, "count");
        assert!(!scalar.to_string().contains("SCALAR_SECRET"));
        let missing = parse_tool_arguments::<Arguments>(json!({"entries":[{}]})).unwrap_err();
        assert_eq!(missing.path, "entries[0]");
        assert!(missing.message.contains("missing field `label`"));
        let unknown =
            parse_tool_arguments::<Arguments>(json!({"entries":[],"unexpected":"SECRET"}))
                .unwrap_err();
        assert_eq!(unknown.message, "unknown field");
        assert_eq!(unknown.path, "unexpected");
        assert!(!unknown.to_string().contains("SECRET"));
    }
}

/// Field-path diagnostic without the argument payload. Conventional missing-field
/// messages retain the field name; other potentially value-bearing messages are
/// reduced to error categories.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tool arguments at {path}: {message}")]
pub struct ToolArgumentError {
    pub path: String,
    pub message: String,
}

/// Decode tool inputs, including array indices in structural error paths.
/// Conventional missing-field errors name the expected field; other Serde
/// messages are reduced to a category instead of echoing unexpected values.
/// Custom deserializers should not put sensitive data in missing-field messages.
pub fn parse_tool_arguments<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolArgumentError> {
    if !arguments.is_object() {
        return Err(ToolArgumentError {
            path: ".".into(),
            message: "tool arguments must be a JSON object".into(),
        });
    }
    serde_path_to_error::deserialize(arguments).map_err(|error| {
        let message = error.inner().to_string();
        let message = if message.starts_with("missing field `") {
            message
        } else if message.starts_with("unknown field ") {
            "unknown field".into()
        } else if message.starts_with("invalid type:") {
            "invalid type".into()
        } else if message.starts_with("invalid value:") {
            "invalid value".into()
        } else {
            "argument deserialization failed".into()
        };
        ToolArgumentError {
            path: error.path().to_string(),
            message,
        }
    })
}
