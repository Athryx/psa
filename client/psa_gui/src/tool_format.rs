//! Presentation of tool arguments as Python-style keyword arguments.
use serde_json::Value;

/// Arguments beyond this size are summarized without a full JSON parse.
const SUMMARY_PARSE_BYTES: usize = 64 * 1024;
const SUMMARY_KWARGS_CHARS: usize = 160;
const SUMMARY_STRING_CHARS: usize = 48;
const PARTIAL_CODE_BYTES: usize = 8 * 1024;
const PREVIEW_LINES: usize = 5;
const PREVIEW_LINE_CHARS: usize = 240;
/// Strings longer than this, or spanning lines, read better outside the kwargs.
const INLINE_STRING_CHARS: usize = 80;

pub(crate) struct ToolArguments {
    pub kwargs: String,
    pub code: Option<(String, &'static str)>,
    /// Long or multi-line strings, named by their path within the arguments.
    pub text: Vec<TextArgument>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextArgument {
    pub name: String,
    pub text: String,
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

/// The argument holding a tool's source code, with its highlighting language.
fn code_field(tool: &str) -> Option<(&'static str, &'static str)> {
    match tool {
        "bash" => Some(("command", "bash")),
        "codemode" => Some(("code", "python")),
        _ => None,
    }
}

/// Move long and multi-line strings out of `value`, in document order.
/// Returns whether nothing of the value is left to show inline.
fn extract_text_arguments(name: &str, value: &mut Value, text: &mut Vec<TextArgument>) -> bool {
    match value {
        Value::String(content) => {
            let is_inline =
                !content.contains('\n') && !truncate_chars(content, INLINE_STRING_CHARS).1;
            if !is_inline {
                text.push(TextArgument {
                    name: name.to_owned(),
                    text: std::mem::take(content),
                });
            }
            !is_inline
        }
        Value::Array(items) if !items.is_empty() => {
            let mut index = 0;
            items.retain_mut(|item| {
                index += 1;
                !extract_text_arguments(&format!("{name}[{}]", index - 1), item, text)
            });
            items.is_empty()
        }
        Value::Object(fields) if !fields.is_empty() => {
            fields.retain(|key, field| {
                !extract_text_arguments(&format!("{name}.{key}"), field, text)
            });
            fields.is_empty()
        }
        _ => false,
    }
}

pub(crate) fn format_arguments(tool: &str, arguments: &str) -> ToolArguments {
    let Ok(Value::Object(mut values)) = serde_json::from_str(arguments) else {
        return ToolArguments {
            kwargs: arguments.to_owned(),
            code: None,
            text: Vec::new(),
        };
    };
    let code_field = code_field(tool);
    let code = code_field.and_then(|(field, language)| {
        if values.get(field).is_some_and(Value::is_string) {
            values
                .remove(field)
                .and_then(|value| value.as_str().map(|text| (text.to_owned(), language)))
        } else {
            None
        }
    });
    let mut text = Vec::new();
    values.retain(|key, value| !extract_text_arguments(key, value, &mut text));

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
    ToolArguments { kwargs, code, text }
}

/// One-line presentation of a call for a collapsed transcript row.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CallSummary {
    /// The file a call acts on, shown ahead of its other arguments.
    pub target: Option<String>,
    /// Remaining arguments, with long values elided.
    pub kwargs: String,
    pub code: Option<(String, &'static str)>,
}

fn truncate_chars(text: &str, limit: usize) -> (&str, bool) {
    match text.char_indices().nth(limit) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    }
}

fn preview_value(value: &Value) -> String {
    match value {
        Value::String(text) => {
            let (prefix, truncated) = truncate_chars(text, SUMMARY_STRING_CHARS);
            let mut quoted = serde_json::to_string(prefix).unwrap_or_default();
            if truncated {
                quoted.insert(quoted.len() - 1, '…');
            }
            quoted
        }
        Value::Array(values) if values.len() > 4 || values.iter().any(is_container) => {
            let noun = if values.len() == 1 { "item" } else { "items" };
            format!("[{} {noun}]", values.len())
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(preview_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) if !values.is_empty() => {
            let noun = if values.len() == 1 { "key" } else { "keys" };
            format!("{{{} {noun}}}", values.len())
        }
        _ => python_value(value),
    }
}

fn is_container(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

/// Decode the start of a string field from JSON that may still be streaming.
///
/// This is a textual search, not a parse: it serves previews of arguments that
/// are incomplete or too large to parse on every render.
fn parse_partial_string_field(arguments: &str, field: &str, limit: usize) -> Option<String> {
    let key = format!("\"{field}\"");
    let after_key = &arguments[arguments.find(&key)? + key.len()..];
    let encoded = after_key
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;

    let mut text = String::new();
    let mut chars = encoded.chars();
    while text.len() < limit {
        match chars.next() {
            None | Some('"') => break,
            Some('\\') => match chars.next() {
                Some('n') => text.push('\n'),
                Some('t') => text.push('\t'),
                Some('r') => text.push('\r'),
                // A cut-off or unpaired escape ends what can be previewed.
                Some('u') => match parse_unicode_escape(&mut chars) {
                    Some(decoded) => text.push(decoded),
                    None => break,
                },
                Some('b' | 'f') | None => {}
                Some(other) => text.push(other),
            },
            Some(other) => text.push(other),
        }
    }
    Some(text)
}

/// Decode the hex digits following `\u`, joining a surrogate pair when present.
fn parse_unicode_escape(chars: &mut std::str::Chars<'_>) -> Option<char> {
    fn parse_code_unit(chars: &mut std::str::Chars<'_>) -> Option<u32> {
        let unit = u32::from_str_radix(chars.as_str().get(..4)?, 16).ok()?;
        chars.nth(3);
        Some(unit)
    }

    let high = parse_code_unit(chars)?;
    if !(0xD800..0xDC00).contains(&high) {
        return char::from_u32(high);
    }
    let mut rest = chars.clone();
    if rest.next() != Some('\\') || rest.next() != Some('u') {
        return None;
    }
    let low = parse_code_unit(&mut rest)?;
    *chars = rest;
    char::from_u32(0x10000 + ((high - 0xD800) << 10) + low.checked_sub(0xDC00)?)
}

pub(crate) fn summarize_call(tool: &str, arguments: &str) -> CallSummary {
    let code_field = code_field(tool);
    let parsed = (arguments.len() <= SUMMARY_PARSE_BYTES)
        .then(|| serde_json::from_str(arguments).ok())
        .flatten();
    let Some(Value::Object(mut values)) = parsed else {
        let code = code_field.and_then(|(field, language)| {
            parse_partial_string_field(arguments, field, PARTIAL_CODE_BYTES)
                .map(|code| (code, language))
        });
        let kwargs = if code.is_some() {
            String::new()
        } else {
            let (prefix, truncated) = truncate_chars(arguments, SUMMARY_KWARGS_CHARS);
            format!("{prefix}{}", if truncated { "…" } else { "" })
        };
        return CallSummary {
            target: None,
            kwargs,
            code,
        };
    };

    let mut take_string = |field: &str| match values.remove(field) {
        Some(Value::String(text)) => Some(text),
        Some(other) => {
            values.insert(field.to_owned(), other);
            None
        }
        None => None,
    };
    let code =
        code_field.and_then(|(field, language)| take_string(field).map(|code| (code, language)));
    let target = take_string("path").or_else(|| take_string("file_path"));

    let mut kwargs = String::new();
    for (key, value) in &values {
        if kwargs.chars().count() > SUMMARY_KWARGS_CHARS {
            kwargs.push_str(", …");
            break;
        }
        if !kwargs.is_empty() {
            kwargs.push_str(", ");
        }
        if is_keyword_name(key) {
            kwargs.push_str(&format!("{key}={}", preview_value(value)));
        } else {
            let (key, _) = truncate_chars(key, SUMMARY_STRING_CHARS);
            kwargs.push_str(&format!("**{{{key:?}: {}}}", preview_value(value)));
        }
    }
    CallSummary {
        target,
        kwargs,
        code,
    }
}

/// The leading non-blank lines of a tool's source, shown without expanding the call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CodePreview {
    pub text: String,
    pub hidden_lines: usize,
}

pub(crate) fn build_code_preview(code: &str) -> CodePreview {
    let mut lines = code.lines().filter(|line| !line.trim().is_empty());
    let mut text = String::new();
    for line in lines.by_ref().take(PREVIEW_LINES) {
        if !text.is_empty() {
            text.push('\n');
        }
        let (prefix, truncated) = truncate_chars(line, PREVIEW_LINE_CHARS);
        text.push_str(prefix);
        if truncated {
            text.push('…');
        }
    }
    CodePreview {
        text,
        hidden_lines: lines.count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_nested_python_values_and_unusual_keys() {
        let result = format_arguments(
            "read",
            r#"{"path":"a\tb", "options":[true,null,{"x":false}], "bad-key":1}"#,
        );
        assert!(result.kwargs.contains(r#"path="a\tb""#));
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

    #[test]
    fn long_and_multiline_strings_leave_the_kwargs_under_their_argument_path() {
        let long = "x".repeat(INLINE_STRING_CHARS + 1);
        let arguments = serde_json::json!({
            "path": "src/main.rs",
            "edits": [
                {"old_text": "a\nb", "new_text": "c\nd"},
                {"old_text": "short", "new_text": long},
            ],
            "notes": ["one\ntwo"],
            "empty": {"list": [], "map": {}},
        })
        .to_string();
        let result = format_arguments("edit", &arguments);
        let text: Vec<_> = result
            .text
            .iter()
            .map(|argument| (argument.name.as_str(), argument.text.as_str()))
            .collect();
        assert_eq!(
            text,
            [
                ("edits[0].new_text", "c\nd"),
                ("edits[0].old_text", "a\nb"),
                ("edits[1].new_text", long.as_str()),
                ("notes[0]", "one\ntwo"),
            ]
        );
        assert_eq!(
            result.kwargs,
            r#"edits=[{"old_text": "short"}], empty={"list": [], "map": {}}, path="src/main.rs""#
        );

        let result = format_arguments("bash", r#"{"command":"a\nb", "stdin":"c\nd"}"#);
        assert_eq!(result.code, Some(("a\nb".into(), "bash")));
        assert_eq!(result.kwargs, "");
        assert_eq!(result.text.len(), 1);
    }

    #[test]
    fn summaries_lead_with_the_target_and_elide_long_values() {
        let content = "x".repeat(500);
        let arguments = serde_json::json!({
            "path": "src/main.rs", "content": content, "edits": [{"old_text": "a"}],
            "lines": [1, 2], "bad-key": {},
        })
        .to_string();
        let summary = summarize_call("write", &arguments);
        assert_eq!(summary.target.as_deref(), Some("src/main.rs"));
        assert_eq!(summary.code, None);
        assert!(summary.kwargs.contains("edits=[1 item]"), "{summary:?}");
        assert!(summary.kwargs.contains("lines=[1, 2]"), "{summary:?}");
        assert!(
            summary.kwargs.contains(r#"**{"bad-key": {}}"#),
            "{summary:?}"
        );
        assert!(
            summary
                .kwargs
                .contains(&format!("content=\"{}…\"", "x".repeat(48)))
        );
        assert!(summary.kwargs.len() < 250, "{summary:?}");

        let summary = summarize_call("read", r#"{"path": 4, "offset": 10}"#);
        assert_eq!(summary.target, None);
        assert_eq!(summary.kwargs, "offset=10, path=4");
    }

    #[test]
    fn summaries_recover_code_from_streaming_and_oversized_arguments() {
        let summary = summarize_call("bash", r#"{"command":"echo hi", "yield_time_ms":5}"#);
        assert_eq!(summary.code, Some(("echo hi".into(), "bash")));
        assert_eq!(summary.kwargs, "yield_time_ms=5");

        let streaming = r#"{"code": "print(\"é\u00e9\ud83d\udc0d\")\nfor x in y:\n\tpa"#;
        let summary = summarize_call("codemode", streaming);
        assert_eq!(
            summary.code,
            Some(("print(\"éé🐍\")\nfor x in y:\n\tpa".into(), "python"))
        );
        assert_eq!(summary.kwargs, "");
        for incomplete in [
            r#"{"code": "a\"#,
            r#"{"code": "a\u00"#,
            r#"{"code": "a\ud83d\u"#,
        ] {
            assert_eq!(
                summarize_call("codemode", incomplete).code,
                Some(("a".into(), "python")),
                "{incomplete}"
            );
        }

        let oversized = serde_json::json!({ "command": "true\n".repeat(SUMMARY_PARSE_BYTES) });
        let summary = summarize_call("bash", &oversized.to_string());
        let (code, _) = summary.code.unwrap();
        assert!(code.starts_with("true\ntrue\n") && code.len() <= PARTIAL_CODE_BYTES + 4);

        let summary = summarize_call("bash", "{broken");
        assert_eq!((summary.code, summary.kwargs.as_str()), (None, "{broken"));
        let summary = summarize_call("read", &"y".repeat(1000));
        assert_eq!(summary.kwargs.chars().count(), SUMMARY_KWARGS_CHARS + 1);
    }

    #[test]
    fn code_previews_keep_leading_lines_and_count_the_rest() {
        assert_eq!(
            build_code_preview("\n\n  cargo test\n\n  --lib\n\n"),
            CodePreview {
                text: "  cargo test\n  --lib".into(),
                hidden_lines: 0
            }
        );
        let preview = build_code_preview(&"line\n".repeat(PREVIEW_LINES + 3));
        assert_eq!(preview.text.lines().count(), PREVIEW_LINES);
        assert_eq!(preview.hidden_lines, 3);
        let preview = build_code_preview(&"é".repeat(PREVIEW_LINE_CHARS + 10));
        assert_eq!(preview.text.chars().count(), PREVIEW_LINE_CHARS + 1);
        assert!(preview.text.ends_with('…'));
    }
}
