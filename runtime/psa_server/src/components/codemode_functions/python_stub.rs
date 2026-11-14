//! Render codemode function schemas as Python stubs the model can call directly.
//!
//! Only the JSON Schema subset generated for tool arguments is rendered
//! precisely; anything else degrades to `Any` rather than failing.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// One Python function stub plus the `TypedDict` classes its arguments reference.
pub(crate) struct PythonStub {
    pub classes: BTreeSet<String>,
    pub function: String,
}

pub(crate) fn render_python_stub(
    name: &str,
    description: &str,
    parameters: &Value,
    is_async: bool,
    returns: &str,
) -> PythonStub {
    let mut renderer = Renderer {
        defs: parameters.get("$defs").and_then(Value::as_object),
        classes: BTreeSet::new(),
    };
    let properties = parameters.get("properties").and_then(Value::as_object);
    let required = required_names(parameters);

    let mut arguments = Vec::new();
    let mut documented = Vec::new();
    // Required parameters first so that the signature is valid Python.
    let fields = properties.into_iter().flatten();
    let (mandatory, optional): (Vec<_>, Vec<_>) =
        fields.partition(|(field, _)| required.contains(field.as_str()));
    for (field, schema) in mandatory.into_iter().chain(optional) {
        let mut annotation = renderer.python_type(schema);
        let argument = if required.contains(field.as_str()) {
            format!("{field}: {annotation}")
        } else {
            let default = schema
                .get("default")
                .map_or_else(|| "None".to_owned(), python_literal);
            if default == "None" && !annotation.split(" | ").any(|part| part == "None") {
                annotation.push_str(" | None");
            }
            format!("{field}: {annotation} = {default}")
        };
        arguments.push(argument);
        if let Some(text) = schema.get("description").and_then(Value::as_str) {
            documented.push(format!("{field}: {}", single_line(text)));
        }
    }

    let prefix = if is_async { "async def" } else { "def" };
    let signature = if arguments.is_empty() {
        format!("{prefix} {name}() -> {returns}:")
    } else {
        format!("{prefix} {name}(*, {}) -> {returns}:", arguments.join(", "))
    };

    let mut docstring: Vec<String> = description.trim().lines().map(str::to_owned).collect();
    if !documented.is_empty() {
        docstring.push(String::new());
        docstring.extend(documented);
    }
    let body = docstring
        .iter()
        .map(|line| match line.trim_end() {
            "" => String::new(),
            line => format!("    {}", escape_docstring(line)),
        })
        .collect::<Vec<_>>()
        .join("\n");

    PythonStub {
        classes: renderer.classes,
        function: format!("{signature}\n    \"\"\"\n{body}\n    \"\"\""),
    }
}

/// Render a result class as its documented fields only; the methods that make
/// `str()` match the tool's text are an implementation detail.
pub(crate) fn render_result_class(name: &str, schema: &Value) -> BTreeSet<String> {
    let mut renderer = Renderer {
        defs: schema.get("$defs").and_then(Value::as_object),
        classes: BTreeSet::new(),
    };
    let properties = schema.get("properties").and_then(Value::as_object);

    let mut lines = vec![format!("class {name}:")];
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        lines.push(format!(
            "    \"\"\"{}\"\"\"",
            escape_docstring(&single_line(description))
        ));
    }
    for (field, field_schema) in properties.into_iter().flatten() {
        let annotation = renderer.python_type(field_schema);
        let comment = field_schema
            .get("description")
            .and_then(Value::as_str)
            .map(|text| format!("  # {}", single_line(text)))
            .unwrap_or_default();
        lines.push(format!("    {field}: {annotation}{comment}"));
    }

    renderer.classes.insert(lines.join("\n"));
    renderer.classes
}

struct Renderer<'a> {
    defs: Option<&'a Map<String, Value>>,
    classes: BTreeSet<String>,
}

impl Renderer<'_> {
    fn python_type(&mut self, schema: &Value) -> String {
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            return self.referenced_type(reference);
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let literals: Vec<_> = values.iter().map(python_literal).collect();
            return format!("Literal[{}]", literals.join(", "));
        }
        if let Some(value) = schema.get("const") {
            return format!("Literal[{}]", python_literal(value));
        }
        for keyword in ["anyOf", "oneOf"] {
            if let Some(options) = schema.get(keyword).and_then(Value::as_array) {
                let types = options
                    .iter()
                    .map(|option| self.python_type(option))
                    .collect();
                return join_union(types);
            }
        }
        match schema.get("type") {
            Some(Value::String(kind)) => self.named_type(kind, schema),
            Some(Value::Array(kinds)) => {
                let types = kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|kind| self.named_type(kind, schema))
                    .collect();
                join_union(types)
            }
            _ => "Any".into(),
        }
    }

    fn named_type(&mut self, kind: &str, schema: &Value) -> String {
        match kind {
            "string" => "str".into(),
            "integer" => "int".into(),
            "number" => "float".into(),
            "boolean" => "bool".into(),
            "null" => "None".into(),
            "array" => match schema.get("items") {
                Some(items) => format!("list[{}]", self.python_type(items)),
                None => "list".into(),
            },
            "object" => "dict".into(),
            _ => "Any".into(),
        }
    }

    fn referenced_type(&mut self, reference: &str) -> String {
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            return "Any".into();
        };
        let Some(definition) = self.defs.and_then(|defs| defs.get(name)) else {
            return "Any".into();
        };
        let Some(properties) = definition.get("properties").and_then(Value::as_object) else {
            return self.python_type(definition);
        };

        let required = required_names(definition);
        let mut fields = Vec::new();
        for (field, schema) in properties {
            let annotation = self.python_type(schema);
            let annotation = if required.contains(field.as_str()) {
                annotation
            } else {
                format!("NotRequired[{annotation}]")
            };
            let comment = schema
                .get("description")
                .and_then(Value::as_str)
                .map(|text| format!("  # {}", single_line(text)))
                .unwrap_or_default();
            fields.push(format!("    {field}: {annotation}{comment}"));
        }
        self.classes
            .insert(format!("class {name}(TypedDict):\n{}", fields.join("\n")));
        name.to_owned()
    }
}

fn required_names(schema: &Value) -> BTreeSet<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn join_union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for kind in types {
        if !unique.contains(&kind) {
            unique.push(kind);
        }
    }
    // Keep None last, matching the usual `T | None` spelling.
    if let Some(index) = unique.iter().position(|kind| kind == "None") {
        let none = unique.remove(index);
        unique.push(none);
    }
    unique.join(" | ")
}

fn python_literal(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_docstring(text: &str) -> String {
    text.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_keyword_only_signature_with_defaults_and_docs() {
        let stub = render_python_stub(
            "read",
            "Read a file.",
            &json!({
                "type": "object",
                "properties": {
                    "index": {"type": "integer", "default": -1},
                    "limit": {"type": "integer", "description": "Maximum lines"},
                    "path": {"type": "string", "description": "File path"}
                },
                "required": ["path"]
            }),
            true,
            "str",
        );
        assert_eq!(
            stub.function,
            concat!(
                "async def read(*, path: str, index: int = -1, limit: int | None = None) -> str:\n",
                "    \"\"\"\n",
                "    Read a file.\n",
                "\n",
                "    path: File path\n",
                "    limit: Maximum lines\n",
                "    \"\"\""
            )
        );
        assert!(stub.classes.is_empty());
    }

    #[test]
    fn renders_referenced_objects_as_typed_dicts_and_enums_as_literals() {
        let stub = render_python_stub(
            "edit",
            "Edit.",
            &json!({
                "$defs": {"Replacement": {
                    "type": "object",
                    "properties": {
                        "newText": {"type": "string"},
                        "oldText": {"type": "string", "description": "Exact\n text"}
                    },
                    "required": ["oldText"]
                }},
                "properties": {
                    "edits": {"type": "array", "items": {"$ref": "#/$defs/Replacement"}},
                    "flag": {"type": ["boolean", "null"]},
                    "mode": {"enum": ["a", "b"]}
                },
                "required": ["edits", "mode"]
            }),
            false,
            "str",
        );
        assert!(stub.function.starts_with(
            "def edit(*, edits: list[Replacement], mode: Literal[\"a\", \"b\"], flag: bool | None = None) -> str:"
        ), "{}", stub.function);
        assert_eq!(
            stub.classes.into_iter().collect::<Vec<_>>(),
            [
                "class Replacement(TypedDict):\n    newText: NotRequired[str]\n    oldText: str  # Exact text"
            ]
        );
    }

    #[test]
    fn functions_without_arguments_and_docstring_quotes_stay_valid() {
        let stub = render_python_stub(
            "noop",
            "Say \"\"\"hi\"\"\".",
            &json!({"type": "object"}),
            true,
            "str",
        );
        assert!(stub.function.starts_with("async def noop() -> str:"));
        assert!(!stub.function.contains("Say \"\"\""));
    }
}
