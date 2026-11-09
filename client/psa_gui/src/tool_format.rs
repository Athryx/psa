//! Presentation of tool arguments as Python-style keyword arguments.
use serde_json::Value;

pub(crate) struct ToolArguments {
    pub kwargs: String,
    pub code: Option<(String, &'static str)>,
}

fn python_value(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(value) => if *value { "True" } else { "False" }.into(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_value(&Value::String(key.clone())),
                    python_value(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn is_keyword_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        && !matches!(
            name,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        )
}

pub(crate) fn format_arguments(tool: &str, arguments: &str) -> ToolArguments {
    let Ok(Value::Object(mut values)) = serde_json::from_str(arguments) else {
        return ToolArguments {
            kwargs: arguments.to_owned(),
            code: None,
        };
    };
    let code_field = match tool {
        "bash" => Some(("command", "bash")),
        "codemode" => Some(("code", "python")),
        _ => None,
    };
    let code = code_field.and_then(|(field, language)| {
        if values.get(field).is_some_and(Value::is_string) {
            values
                .remove(field)
                .and_then(|value| value.as_str().map(|text| (text.to_owned(), language)))
        } else {
            None
        }
    });
    let kwargs = values
        .iter()
        .map(|(key, value)| {
            if is_keyword_name(key) {
                format!("{key}={}", python_value(value))
            } else {
                format!(
                    "**{{{}: {}}}",
                    python_value(&Value::String(key.clone())),
                    python_value(value)
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    ToolArguments { kwargs, code }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_nested_python_values_and_unusual_keys() {
        let result = format_arguments(
            "read",
            r#"{"path":"a\nb", "options":[true,null,{"x":false}], "bad-key":1}"#,
        );
        assert!(result.kwargs.contains(r#"path="a\nb""#));
        assert!(
            result
                .kwargs
                .contains(r#"options=[True, None, {"x": False}]"#)
        );
        assert!(result.kwargs.contains(r#"**{"bad-key": 1}"#));
    }

    #[test]
    fn separates_only_string_code_and_preserves_bad_input() {
        let result = format_arguments("bash", r#"{"command":"echo hi", "yield_time_ms":5}"#);
        assert_eq!(result.kwargs, "yield_time_ms=5");
        assert_eq!(result.code, Some(("echo hi".into(), "bash")));
        assert_eq!(
            format_arguments("codemode", r#"{"code":"print(1)"}"#)
                .code
                .unwrap()
                .1,
            "python"
        );
        assert_eq!(
            format_arguments("bash", r#"{"command":4}"#).kwargs,
            "command=4"
        );
        assert_eq!(format_arguments("bash", "{broken").kwargs, "{broken");
    }
}
