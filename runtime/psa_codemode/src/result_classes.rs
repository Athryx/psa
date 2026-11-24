//! Python prelude that turns structured tool results into objects.
//!
//! Monty cannot run host-defined `__str__`, so result classes are Python code
//! fed before each script. A function with a result type is defined as a
//! Python wrapper that awaits the host function under a private name and wraps
//! the returned fields; `str()` of the object is the tool's text output.

use std::collections::BTreeSet;

use serde_json::Value;

/// Prefix of the private host name a result wrapper calls.
pub const HOST_CALL_PREFIX: &str = "_codemode_host_";
/// Field carrying the tool's text inside a structured result.
pub const TEXT_FIELD: &str = "_text";

const HELPERS: &str = r#"def _codemode_result_init(self, data):
    for key, value in data.items():
        setattr(self, key, value)

def _codemode_result_repr(self, fields):
    parts = []
    for key in fields:
        value = getattr(self, key)
        if isinstance(value, str) and len(value) > 60:
            parts.append(f"{key}=<{len(value)} chars>")
        else:
            parts.append(f"{key}={value!r}")
    return f"{type(self).__name__}({', '.join(parts)})"
"#;

/// The host function a call names: wrappers call it under the private prefix.
pub fn host_function_name(name: &str) -> &str {
    name.strip_prefix(HOST_CALL_PREFIX).unwrap_or(name)
}

/// A host function whose structured result is exposed as a Python class.
pub struct ResultWrapper<'a> {
    pub function: &'a str,
    pub class: &'a str,
    /// JSON Schema of the result object; its properties become the fields.
    pub schema: &'a Value,
}

/// Python source defining every result class and wrapper, or `None` when no
/// function returns a structured result.
pub fn build_result_prelude<'a>(
    wrappers: impl IntoIterator<Item = ResultWrapper<'a>>,
) -> Option<String> {
    let mut classes = BTreeSet::new();
    let mut definitions = Vec::new();
    for wrapper in wrappers {
        let ResultWrapper {
            function,
            class,
            schema,
        } = wrapper;
        if classes.insert(class) {
            definitions.push(build_result_class(class, schema));
        }
        definitions.push(format!(
            r#"async def {function}(*args, **kwargs):
    return {class}(await {HOST_CALL_PREFIX}{function}(*args, **kwargs))
"#
        ));
    }
    if definitions.is_empty() {
        return None;
    }

    let mut source = String::from(HELPERS);
    for definition in definitions {
        source.push('\n');
        source.push_str(&definition);
    }
    Some(source)
}

fn build_result_class(name: &str, schema: &Value) -> String {
    // Debug formatting quotes the field names as Python string literals.
    let fields: String = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys())
        .map(|field| format!("{field:?}, "))
        .collect();
    format!(
        r#"class {name}:
    def __init__(self, data):
        _codemode_result_init(self, data)

    def __str__(self):
        return self.{TEXT_FIELD}

    def __repr__(self):
        return _codemode_result_repr(self, ({fields}))
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wrappers_need_no_prelude() {
        assert_eq!(build_result_prelude([]), None);
    }

    #[test]
    fn shared_classes_are_defined_once_with_one_wrapper_per_function() {
        let schema = serde_json::json!({"properties": {"name": {"type": "string"}}});
        let prelude =
            build_result_prelude(["bash", "interact_bash"].map(|function| ResultWrapper {
                function,
                class: "BashResult",
                schema: &schema,
            }))
            .unwrap();
        assert_eq!(prelude.matches("class BashResult:").count(), 1);
        assert!(prelude.contains(
            "async def bash(*args, **kwargs):\n    return BashResult(await _codemode_host_bash(*args, **kwargs))"
        ));
        assert!(prelude.contains("async def interact_bash("));
    }

    #[test]
    fn classes_list_schema_fields_and_strings_render_text() {
        let schema = serde_json::json!({
            "properties": {"name": {"type": "string"}, "running": {"type": "boolean"}}
        });
        let class = build_result_class("BashResult", &schema);
        assert!(class.starts_with("class BashResult:\n"));
        assert!(class.contains("return self._text"));
        assert!(class.contains(r#"_codemode_result_repr(self, ("name", "running", ))"#));
        assert_eq!(host_function_name("_codemode_host_bash"), "bash");
        assert_eq!(host_function_name("bash"), "bash");
    }
}
