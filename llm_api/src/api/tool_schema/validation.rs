//! Offline instance validation, separate from provider schema pass-through checks.

use std::{collections::HashSet, io, sync::Arc};

use serde_json::Value;

use super::ToolSchema;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_INSTANCE_BYTES: usize = 1024 * 1024;
const MAX_INSTANCE_DEPTH: usize = 128;
const MAX_PATH_CHARS: usize = 256;
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// A bounded diagnostic containing locations and a category, never instance values.
/// Paths are JSON Pointers (the empty string denotes the root), truncated when
/// necessary. Object member names can appear in paths; payload values never do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (instance path: {instance_path}, schema path: {schema_path})")]
pub struct SchemaValidationError {
    pub instance_path: String,
    pub schema_path: String,
    pub message: String,
}

impl SchemaValidationError {
    fn new(instance_path: &str, schema_path: &str, message: &str) -> Self {
        Self {
            instance_path: bounded_path(instance_path),
            schema_path: bounded_path(schema_path),
            message: message.into(),
        }
    }
}

/// An owned, reusable Draft 2020-12 validator. Clones share the compiled engine.
#[derive(Debug, Clone)]
pub struct CompiledToolSchema {
    validator: Arc<jsonschema::Validator>,
}

impl ToolSchema {
    /// Compile a dynamic object-input schema without network or filesystem access.
    ///
    /// Only Draft 2020-12 (or an omitted `$schema`) and local JSON Pointer refs
    /// are supported. Resource IDs, anchors, and dynamic references are rejected.
    /// Compilation does not change the canonical raw/generated schema or the
    /// separate provider-facing `to_openai_parameters` validation policy.
    pub fn compile(&self) -> Result<CompiledToolSchema, SchemaValidationError> {
        let schema = self.as_value();
        validate_limits(schema, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH)
            .map_err(|message| SchemaValidationError::new("", "", message))?;
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(SchemaValidationError::new(
                "",
                "/type",
                "tool schema must have an explicit object root type",
            ));
        }
        validate_schema_resources(schema)?;

        // Both safeguards are intentional: no retrieval features are enabled in
        // Cargo, and offline mode refuses retrieval even if features are unified
        // with another consumer of jsonschema in the same workspace.
        let validator = jsonschema::draft202012::options()
            .offline()
            .build(schema)
            .map_err(|error| {
                SchemaValidationError::new(
                    "",
                    error.instance_path().as_str(),
                    "invalid tool schema",
                )
            })?;
        Ok(CompiledToolSchema {
            validator: Arc::new(validator),
        })
    }
}

impl CompiledToolSchema {
    /// Validate an instance, reporting only the first failure and no payload.
    /// The encoded instance is limited to 1 MiB and its JSON depth to 128.
    pub fn validate(&self, instance: &Value) -> Result<(), SchemaValidationError> {
        validate_limits(instance, MAX_INSTANCE_BYTES, MAX_INSTANCE_DEPTH)
            .map_err(|message| SchemaValidationError::new("", "", message))?;
        self.validator.validate(instance).map_err(|error| {
            // Do not Display/Debug the engine error: many keyword diagnostics
            // include the submitted value (including nested combinator errors).
            SchemaValidationError::new(
                error.instance_path().as_str(),
                error.schema_path().as_str(),
                "tool instance does not satisfy schema",
            )
        })
    }
}

fn bounded_path(path: &str) -> String {
    let mut chars = path.chars();
    let mut result: String = chars.by_ref().take(MAX_PATH_CHARS).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

/// Apply depth bounds before invoking recursive serialization or the engine.
fn validate_limits(value: &Value, max_bytes: usize, max_depth: usize) -> Result<(), &'static str> {
    fn validate_depth(value: &Value, depth: usize, max: usize) -> bool {
        if depth > max {
            return false;
        }
        match value {
            Value::Array(values) => values.iter().all(|v| validate_depth(v, depth + 1, max)),
            Value::Object(values) => values.values().all(|v| validate_depth(v, depth + 1, max)),
            _ => true,
        }
    }

    if !validate_depth(value, 0, max_depth) {
        return Err("JSON depth limit exceeded");
    }
    // Count serialized bytes without allocating a second copy of the payload.
    struct LimitedWriter(usize);
    impl io::Write for LimitedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::Error::other("JSON size limit exceeded"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(LimitedWriter(max_bytes), value)
        .map_err(|_| "JSON encoded size limit exceeded")
}

/// Visit schema positions only, not enum/const/default/examples/extension data.
/// Reference targets become schema positions, even inside an annotation value.
fn validate_schema_resources(root: &Value) -> Result<(), SchemaValidationError> {
    let mut pending = vec![(root, String::new())];
    let mut visited = HashSet::new();
    while let Some((schema, path)) = pending.pop() {
        if !visited.insert(schema as *const Value) {
            continue;
        }
        let Some(object) = schema.as_object() else {
            // The engine checks schema shapes and keyword values.
            continue;
        };
        for keyword in object.keys() {
            let keyword_path = append_pointer(&path, keyword);
            if matches!(
                keyword.as_str(),
                "$id" | "$anchor" | "$recursiveRef" | "$recursiveAnchor"
            ) || keyword.starts_with("$dynamic")
            {
                return Err(SchemaValidationError::new(
                    "",
                    &keyword_path,
                    "schema resource IDs, anchors and dynamic references are unsupported",
                ));
            }
            if keyword == "$schema"
                && !matches!(object[keyword].as_str(), Some(uri) if uri == DIALECT || uri == format!("{DIALECT}#"))
            {
                return Err(SchemaValidationError::new(
                    "",
                    &keyword_path,
                    "only the Draft 2020-12 schema dialect is supported",
                ));
            }
        }
        if let Some(reference) = object.get("$ref") {
            let reference_path = append_pointer(&path, "$ref");
            let target = reference.as_str().and_then(parse_local_pointer);
            let Some(pointer) = target else {
                return Err(SchemaValidationError::new(
                    "",
                    &reference_path,
                    "only local JSON Pointer references are supported",
                ));
            };
            let Some(target) = root.pointer(&pointer) else {
                return Err(SchemaValidationError::new(
                    "",
                    &reference_path,
                    "local schema reference does not resolve",
                ));
            };
            pending.push((target, pointer));
        }

        for keyword in [
            "$defs",
            "definitions",
            "properties",
            "patternProperties",
            "dependentSchemas",
        ] {
            if let Some(children) = object.get(keyword).and_then(Value::as_object) {
                let parent = append_pointer(&path, keyword);
                for (name, child) in children {
                    pending.push((child, append_pointer(&parent, name)));
                }
            }
        }
        for keyword in [
            "items",
            "contains",
            "additionalProperties",
            "unevaluatedProperties",
            "unevaluatedItems",
            "propertyNames",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            if let Some(child) = object.get(keyword) {
                pending.push((child, append_pointer(&path, keyword)));
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(children) = object.get(keyword).and_then(Value::as_array) {
                let parent = append_pointer(&path, keyword);
                for (index, child) in children.iter().enumerate() {
                    pending.push((child, append_pointer(&parent, &index.to_string())));
                }
            }
        }
    }
    Ok(())
}

fn append_pointer(parent: &str, token: &str) -> String {
    format!("{parent}/{}", token.replace('~', "~0").replace('/', "~1"))
}

/// Decode a URI fragment into a JSON Pointer; never accept an anchor or URI.
fn parse_local_pointer(reference: &str) -> Option<String> {
    let fragment = reference.strip_prefix('#')?;
    if !fragment.is_empty() && !fragment.starts_with('/') {
        return None;
    }
    let mut bytes = fragment.bytes();
    let mut decoded = Vec::with_capacity(fragment.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)? as u8;
            let low = (bytes.next()? as char).to_digit(16)? as u8;
            decoded.push(high * 16 + low);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn validates_generated_typed_schema_and_preserves_canonical_json() {
        #[derive(Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        struct Input {
            entries: Vec<Entry>,
        }
        #[derive(Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            count: u32,
        }
        let schema = ToolSchema::for_type::<Input>();
        let before = schema.as_value().clone();
        let compiled = schema.compile().unwrap();
        let instance = json!({"entries":[{"count":2}]});
        compiled.clone().validate(&instance).unwrap();
        let decoded: Input = serde_json::from_value(instance).unwrap();
        assert_eq!(decoded.entries[0].count, 2);
        assert!(
            compiled
                .validate(&json!({"entries":[{"count":-1}]}))
                .is_err()
        );
        assert!(
            compiled
                .validate(&json!({"entries":[],"unknown":true}))
                .is_err()
        );
        assert_eq!(schema.as_value(), &before);
        assert_eq!(serde_json::to_value(&schema).unwrap(), before);
        // A schema field in another generated config remains arbitrary JSON.
        #[derive(Deserialize, JsonSchema)]
        struct Config {
            schema: ToolSchema,
        }
        let config_schema = ToolSchema::for_type::<Config>();
        let config_instance = json!({"schema": before});
        config_schema
            .compile()
            .unwrap()
            .validate(&config_instance)
            .unwrap();
        let decoded: Config = serde_json::from_value(config_instance).unwrap();
        assert_eq!(decoded.schema, schema);
    }

    #[test]
    fn local_references_support_nested_and_recursive_schemas() {
        let compiled = ToolSchema::from(json!({
            "type":"object", "properties":{"entry":{"$ref":"#/$defs/a~1b"}},
            "$defs":{"a/b":{"type":"integer","minimum":2}},
            "required":["entry"]
        }))
        .compile()
        .unwrap();
        compiled.validate(&json!({"entry":2})).unwrap();
        let error = compiled.validate(&json!({"entry":1})).unwrap_err();
        assert_eq!(error.instance_path, "/entry");
        assert!(error.schema_path.ends_with("/minimum"));
        assert!(
            ToolSchema::from(json!({
                "type":"object", "properties":{"child":{"$ref":"#"}}
            }))
            .compile()
            .unwrap()
            .validate(&json!({"child":{}}))
            .is_ok()
        );
        assert!(
            ToolSchema::from(json!({
                "type":"object", "properties":{"x":{"$ref":"#/$defs/missing"}}
            }))
            .compile()
            .is_err()
        );
    }

    #[test]
    fn engine_enforces_instance_constraints() {
        let compiled = ToolSchema::from(json!({
            "type":"object", "required":["name","numbers"], "additionalProperties":false,
            "properties":{
                "name":{"type":"string","minLength":2,"pattern":"^[a-z]+$"},
                "numbers":{"type":"array","minItems":1,"uniqueItems":true,
                    "items":{"type":"integer","minimum":1,"maximum":5}},
                "choice":{"oneOf":[{"const":"yes"},{"type":"integer","multipleOf":2}]}
            }
        }))
        .compile()
        .unwrap();
        compiled
            .validate(&json!({"name":"ok","numbers":[1,2],"choice":4}))
            .unwrap();
        for instance in [
            json!({}),
            json!({"name":"x","numbers":[1]}),
            json!({"name":"NO","numbers":[1]}),
            json!({"name":"ok","numbers":[]}),
            json!({"name":"ok","numbers":[1,1]}),
            json!({"name":"ok","numbers":[6]}),
            json!({"name":"ok","numbers":[1],"choice":3}),
            json!({"name":"ok","numbers":[1],"extra":true}),
            json!(null),
            json!([]),
        ] {
            assert!(compiled.validate(&instance).is_err());
        }
    }

    #[test]
    fn rejects_invalid_schemas_dialects_and_non_object_roots() {
        for schema in [
            json!(true),
            json!({}),
            json!({"type":"string"}),
            json!({"type":["object","null"]}),
            json!({"type":"object","required":"bad"}),
            json!({"type":"object","properties":{"x":{"minimum":"bad"}}}),
            json!({"type":"object","properties":{"x":{"pattern":"["}}}),
            json!({"type":"object","$schema":"http://json-schema.org/draft-07/schema#"}),
        ] {
            assert!(ToolSchema::from(schema).compile().is_err());
        }
        assert!(
            ToolSchema::from(json!({"type":"object","$schema":DIALECT}))
                .compile()
                .is_ok()
        );
    }

    #[test]
    fn rejects_external_resources_in_schema_positions_not_literal_data() {
        for reference in [
            "https://example.com/schema",
            "file:///etc/passwd",
            "other.json",
            "#anchor",
        ] {
            assert!(
                ToolSchema::from(json!({
                    "type":"object","$defs":{"unused":{"$ref":reference}}
                }))
                .compile()
                .is_err()
            );
        }
        for keyword in [
            "$id",
            "$anchor",
            "$dynamicAnchor",
            "$dynamicRef",
            "$dynamicFuture",
        ] {
            assert!(
                ToolSchema::from(json!({
                    "type":"object","properties":{"x":{keyword:"untrusted"}}
                }))
                .compile()
                .is_err()
            );
        }
        let literal =
            json!({"$ref":"https://example.com/secret","$id":"literal","$dynamicRef":"literal"});
        let compiled = ToolSchema::from(json!({
            "type":"object", "properties":{"literal":{"enum":[literal.clone()],
                "default":literal.clone(),"examples":[literal.clone()]}},
            "examples":[literal.clone()], "default":literal.clone()
        }))
        .compile()
        .unwrap();
        compiled.validate(&json!({"literal":literal})).unwrap();
        // A pointer into literal data makes its target a schema, not an escape.
        assert!(
            ToolSchema::from(json!({
                "type":"object", "$ref":"#/default", "default":{"$id":"not-allowed"}
            }))
            .compile()
            .is_err()
        );
    }

    #[test]
    fn diagnostics_are_bounded_and_do_not_echo_submitted_values() {
        let compiled = ToolSchema::from(json!({
            "type":"object","properties":{"payload":{"type":"integer"}}
        }))
        .compile()
        .unwrap();
        let error = compiled
            .validate(&json!({"payload":{"secret":"DO_NOT_ECHO"}}))
            .unwrap_err();
        assert_eq!(error.instance_path, "/payload");
        assert_eq!(error.schema_path, "/properties/payload/type");
        assert!(!format!("{error:?} {error}").contains("DO_NOT_ECHO"));
        let long_key = "é".repeat(1000);
        let compiled = ToolSchema::from(json!({
            "type":"object","additionalProperties":{"type":"integer"}
        }))
        .compile()
        .unwrap();
        let error = compiled
            .validate(&json!({long_key:"DO_NOT_ECHO"}))
            .unwrap_err();
        assert!(error.instance_path.chars().count() <= MAX_PATH_CHARS + 1);
        assert!(!format!("{error:?} {error}").contains("DO_NOT_ECHO"));
    }

    #[test]
    fn limits_schema_and_instance_size_and_depth() {
        assert!(
            ToolSchema::from(json!({"type":"object","description":"x".repeat(MAX_SCHEMA_BYTES)}))
                .compile()
                .is_err()
        );
        let mut nested = json!(true);
        for _ in 0..MAX_SCHEMA_DEPTH + 1 {
            nested = json!({"not":nested});
        }
        assert!(
            ToolSchema::from(json!({"type":"object","properties":{"x":nested}}))
                .compile()
                .is_err()
        );
        let compiled = ToolSchema::from(json!({"type":"object"}))
            .compile()
            .unwrap();
        assert!(
            compiled
                .validate(&json!({"x":"s".repeat(MAX_INSTANCE_BYTES)}))
                .is_err()
        );
        let mut nested = json!(null);
        for _ in 0..MAX_INSTANCE_DEPTH + 1 {
            nested = json!([nested]);
        }
        assert!(compiled.validate(&json!({"x":nested})).is_err());
    }
}
