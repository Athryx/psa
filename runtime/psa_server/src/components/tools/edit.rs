use super::{Tool, ToolOutput, ToolSystemPrompt, file_ops, typed_tool_execute};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation, prompts};
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, de::Error};
use std::{ops::Range, time::Duration};
use unicode_normalization::UnicodeNormalization;

pub struct Edit;

#[derive(JsonSchema)]
struct EditArgs {
    /// Path to the file to edit (relative or absolute)
    path: String,
    /// One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.
    edits: Vec<Replacement>,
}

#[derive(Deserialize, JsonSchema)]
struct Replacement {
    /// Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.
    #[serde(rename = "oldText")]
    old_text: String,
    /// Replacement text for this targeted edit.
    #[serde(rename = "newText")]
    new_text: String,
}

// The public schema stays canonical; Pi also prepares these compatibility forms
// before validation. Keeping preparation here preserves the typed tool boundary.
impl<'de> Deserialize<'de> for EditArgs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct CompatibleArgs {
            path: String,
            #[serde(default)]
            edits: serde_json::Value,
            #[serde(default, rename = "oldText")]
            old_text: serde_json::Value,
            #[serde(default, rename = "newText")]
            new_text: serde_json::Value,
        }

        let input = CompatibleArgs::deserialize(deserializer)?;
        let prepared_edits = prepare_compatible_edits(input.edits, input.old_text, input.new_text);
        let edits =
            serde_json::from_value::<Vec<Replacement>>(prepared_edits).map_err(D::Error::custom)?;

        Ok(Self {
            path: input.path,
            edits,
        })
    }
}

fn prepare_compatible_edits(
    mut edits: serde_json::Value,
    old_text: serde_json::Value,
    new_text: serde_json::Value,
) -> serde_json::Value {
    if let Some(encoded) = edits.as_str()
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(encoded)
        && (parsed.is_array() || is_single_replacement(&parsed))
    {
        edits = parsed;
    }
    if is_single_replacement(&edits) {
        edits = serde_json::Value::Array(vec![edits]);
    }

    if let (Some(old_text), Some(new_text)) = (old_text.as_str(), new_text.as_str()) {
        let mut replacements = match edits {
            serde_json::Value::Array(replacements) => replacements,
            _ => Vec::new(),
        };
        replacements.push(serde_json::json!({"oldText": old_text, "newText": new_text}));
        edits = serde_json::Value::Array(replacements);
    }

    edits
}

fn is_single_replacement(value: &serde_json::Value) -> bool {
    value.get("oldText").is_some_and(|value| value.is_string())
        && value.get("newText").is_some_and(|value| value.is_string())
}

impl Edit {
    async fn execute_typed(
        &self,
        arguments: EditArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        if arguments.edits.is_empty() {
            return edit_error(
                "Edit tool input is invalid. edits must contain at least one replacement.",
            );
        }

        let probe = match context.probe() {
            Ok(probe) => probe,
            Err(error) => return edit_error(error.to_string()),
        };
        let path = tokio::select! {
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = file_ops::resolve_path(probe, &arguments.path, &context.session().working_directory) => match result {
                Ok(path) => path,
                Err(error) => return edit_error(error.to_string()),
            },
        };
        let _guard = tokio::select! {
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            guard = file_ops::lock_file(context.session().probe_id, &path) => guard,
        };

        let bytes = tokio::select! {
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = file_ops::read_bytes(probe, &path) => match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    let message = match file_ops::parse_remote_error_code(&error) {
                        Some(code) => format!("Could not edit file: {}. Error code: {code}.", arguments.path),
                        None => file_ops::format_io_error(&error, &path, "open"),
                    };
                    return edit_error(message);
                }
            },
        };

        let content = match edit_bytes(&bytes, &arguments.edits, &arguments.path) {
            Ok(content) => content,
            Err(error) => return edit_error(error),
        };
        if cancellation.is_cancelled() {
            return ToolOutput::error(prompts::tool_interrupted());
        }

        // Keep the shared mutation lock until the write future settles. Forced
        // server cancellation can still drop this future: the probe API does not
        // guarantee remote settlement on drop.
        let result = file_ops::write_bytes(probe, &path, content.as_bytes()).await;
        if cancellation.is_cancelled() {
            return ToolOutput::error(prompts::tool_interrupted());
        }
        if let Err(error) = result {
            return edit_error(file_ops::format_io_error(&error, &path, "open"));
        }

        ToolOutput::success(prompts::text(format!(
            "Successfully replaced {} block(s) in {}.",
            arguments.edits.len(),
            arguments.path
        )))
    }
}

impl Tool for Edit {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::for_type::<EditArgs>(
            "edit",
            "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
            Some(false),
        )
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Make precise file edits with exact text replacement, including multiple disjoint edits in one call".into()),
            rules: vec![
                "Use edit for precise changes (edits[].oldText must match exactly)".into(),
                "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls".into(),
                "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.".into(),
                "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.".into(),
            ],
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    typed_tool_execute!(EditArgs, execute_typed);
}

// Bound tool-authored errors, including arbitrary paths and remote diagnostics.
// Ordinary Pi errors remain byte-for-byte unchanged within this budget.
fn edit_error(message: impl AsRef<str>) -> ToolOutput {
    ToolOutput::error(prompts::text(
        prompts::truncate_middle(message.as_ref(), 50 * 1024).into_owned(),
    ))
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

// ECMAScript trimEnd whitespace (not Rust's slightly different White_Space).
fn is_js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn normalize_for_fuzzy_match(text: &str) -> String {
    text.nfkc()
        .collect::<String>()
        .split('\n')
        .map(|line| line.trim_end_matches(is_js_whitespace))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|ch| match ch {
            '\u{2018}'..='\u{201b}' => '\'',
            '\u{201c}'..='\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            _ => ch,
        })
        .collect()
}

fn edit_bytes(bytes: &[u8], edits: &[Replacement], path: &str) -> Result<String, String> {
    // Node Buffer.toString replaces invalid UTF-8 rather than rejecting it.
    let raw = String::from_utf8_lossy(bytes);
    let (bom, content) = match raw.strip_prefix('\u{feff}') {
        Some(content) => ("\u{feff}", content),
        None => ("", raw.as_ref()),
    };
    let crlf = match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) => crlf < lf,
        _ => false,
    };
    let normalized = normalize_to_lf(content);
    let replacement = apply_edits(&normalized, edits, path)?;

    Ok(format!(
        "{bom}{}",
        if crlf {
            replacement.replace('\n', "\r\n")
        } else {
            replacement
        }
    ))
}

struct MatchedReplacement<'a> {
    edit_index: usize,
    start: usize,
    length: usize,
    new_text: &'a str,
}

fn apply_edits(content: &str, edits: &[Replacement], path: &str) -> Result<String, String> {
    let edits: Vec<_> = edits
        .iter()
        .map(|edit| Replacement {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if edits.len() == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            });
        }
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_needles: Vec<_> = edits
        .iter()
        .map(|edit| normalize_for_fuzzy_match(&edit.old_text))
        .collect();
    let requires_fuzzy_matching = edits
        .iter()
        .zip(&fuzzy_needles)
        .any(|(edit, needle)| !content.contains(&edit.old_text) && fuzzy_content.contains(needle));
    let base = if requires_fuzzy_matching {
        fuzzy_content.as_str()
    } else {
        content
    };

    let mut matches = Vec::with_capacity(edits.len());
    for (i, (edit, needle)) in edits.iter().zip(&fuzzy_needles).enumerate() {
        let found = base
            .find(&edit.old_text)
            .map(|index| (index, edit.old_text.len()))
            .or_else(|| {
                fuzzy_content
                    .find(needle)
                    .map(|index| (index, needle.len()))
            });
        let Some((start, length)) = found else {
            return Err(if edits.len() == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            });
        };

        // JavaScript split counts non-overlapping occurrences. Its empty-string
        // separator counts UTF-16 code units, unlike Rust's split("").
        let occurrences = if needle.is_empty() {
            fuzzy_content.encode_utf16().count().saturating_sub(1)
        } else {
            fuzzy_content.matches(needle).count()
        };
        if occurrences > 1 {
            return Err(if edits.len() == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            });
        }

        matches.push(MatchedReplacement {
            edit_index: i,
            start,
            length,
            new_text: &edit.new_text,
        });
    }

    matches.sort_by_key(|replacement| replacement.start);
    for pair in matches.windows(2) {
        if pair[0].start + pair[0].length > pair[1].start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }

    let result = if requires_fuzzy_matching {
        apply_preserving_lines(content, base, &matches)?
    } else {
        apply_replacements(base, &matches, 0)
    };
    if result == content {
        return Err(if edits.len() == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }

    Ok(result)
}

fn apply_replacements(
    base: &str,
    replacements: &[MatchedReplacement<'_>],
    offset: usize,
) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    for replacement in replacements {
        let start = replacement.start - offset;
        result.push_str(&base[cursor..start]);
        result.push_str(replacement.new_text);
        cursor = start + replacement.length;
    }
    result.push_str(&base[cursor..]);
    result
}

struct ReplacementLineGroup {
    lines: Range<usize>,
    replacements: Range<usize>,
}

fn apply_preserving_lines(
    original: &str,
    base: &str,
    matches: &[MatchedReplacement<'_>],
) -> Result<String, String> {
    let original_lines: Vec<_> = original.split_inclusive('\n').collect();
    let base_lines: Vec<_> = base.split_inclusive('\n').collect();
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }

    let mut offsets = vec![0];
    for line in &base_lines {
        offsets.push(offsets.last().unwrap() + line.len());
    }

    // A group covers all touched lines; disjoint edits on a single line must be
    // applied together so one does not overwrite another's normalized output.
    let mut groups: Vec<ReplacementLineGroup> = Vec::new();
    for (i, replacement) in matches.iter().enumerate() {
        let start = offsets
            .windows(2)
            .position(|span| replacement.start >= span[0] && replacement.start < span[1])
            .ok_or("Replacement range is outside the base content.")?;
        let mut end = start + 1;
        while end < offsets.len() && offsets[end] < replacement.start + replacement.length {
            end += 1;
        }
        if end >= offsets.len() {
            return Err("Replacement range is outside the base content.".into());
        }
        if let Some(group) = groups.last_mut()
            && start < group.lines.end
        {
            group.lines.end = group.lines.end.max(end);
            group.replacements.end = i + 1;
            continue;
        }
        groups.push(ReplacementLineGroup {
            lines: start..end,
            replacements: i..i + 1,
        });
    }

    let mut result = String::new();
    let mut cursor = 0;
    for group in groups {
        for line in &original_lines[cursor..group.lines.start] {
            result.push_str(line);
        }
        result.push_str(&apply_replacements(
            &base[offsets[group.lines.start]..offsets[group.lines.end]],
            &matches[group.replacements],
            offsets[group.lines.start],
        ));
        cursor = group.lines.end;
    }
    for line in &original_lines[cursor..] {
        result.push_str(line);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    fn replacements(pairs: &[(&str, &str)]) -> Vec<Replacement> {
        pairs
            .iter()
            .map(|(old, new)| Replacement {
                old_text: (*old).into(),
                new_text: (*new).into(),
            })
            .collect()
    }

    fn apply(content: &str, pairs: &[(&str, &str)]) -> Result<String, String> {
        edit_bytes(content.as_bytes(), &replacements(pairs), "file")
    }

    #[test]
    fn matching_and_newlines_match_installed_pi_reference_cases() {
        // Captured from Pi's applyEditsToNormalizedContent + BOM/newline handling.
        // No Node/Pi installation is required when running the Rust suite.
        #[derive(Deserialize)]
        struct Case {
            name: String,
            content: String,
            edits: Vec<Replacement>,
            result: Option<String>,
            error: Option<String>,
        }
        let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/pi_edit.json")).unwrap();
        for case in cases {
            let actual = edit_bytes(case.content.as_bytes(), &case.edits, "file");
            let expected = match (case.result, case.error) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(error),
                _ => panic!("invalid reference case: {}", case.name),
            };
            assert_eq!(actual, expected, "Pi reference case: {}", case.name);
        }
    }

    #[test]
    fn errors_with_long_unicode_paths_are_byte_bounded() {
        let path = format!("start{}end", "界".repeat(30_000));
        let message = edit_bytes(b"a", &replacements(&[("b", "c")]), &path).unwrap_err();
        let output = edit_error(&message);
        let [llm_api::api::ContentBlock::Text(block)] = output.content.as_slice() else {
            panic!("expected one text block");
        };
        assert!(output.error);
        assert!(block.text.len() <= 50 * 1024);
        assert!(block.text.contains("[... middle truncated ...]"));
        assert!(
            block
                .text
                .starts_with("Could not find the exact text in start")
        );
        assert!(block.text.ends_with(
            "end. The old text must match exactly including all whitespace and newlines."
        ));
        let short = edit_error("oldText must not be empty in file.");
        let [llm_api::api::ContentBlock::Text(block)] = short.content.as_slice() else {
            panic!("expected one text block");
        };
        assert_eq!(block.text, "oldText must not be empty in file.");
    }

    #[test]
    fn canonical_schema_and_prompt() {
        let definition = Edit.tool_definition();
        let schema = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "edit");
        assert_eq!(definition.strict, Some(false));
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["edits"]["type"], "array");
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("edits")));
        assert!(schema["properties"].get("oldText").is_none());
        assert!(
            definition
                .parameters
                .to_openai_parameters(definition.strict)
                .is_ok()
        );
        assert_eq!(Edit.system_prompt().rules.len(), 4);
    }

    #[test]
    fn compatibility_arguments_leave_schema_canonical() {
        for arguments in [
            json!({"path":"file", "edits":[{"oldText":"a", "newText":"b"}]}),
            json!({"path":"file", "edits":{"oldText":"a", "newText":"b"}}),
            json!({"path":"file", "edits":"[{\"oldText\":\"a\",\"newText\":\"b\"}]"}),
            json!({"path":"file", "edits":"{\"oldText\":\"a\",\"newText\":\"b\"}"}),
            json!({"path":"file", "oldText":"a", "newText":"b"}),
            json!({"path":"file", "edits":false, "oldText":"a", "newText":"b", "extra":1}),
        ] {
            let args = parse_tool_arguments::<EditArgs>(arguments).unwrap();
            assert_eq!(args.path, "file");
            assert_eq!(args.edits.len(), 1);
            assert_eq!(args.edits[0].old_text, "a");
            assert_eq!(args.edits[0].new_text, "b");
        }
        let args = parse_tool_arguments::<EditArgs>(json!({"path":"f", "edits":{"oldText":"a", "newText":"b"}, "oldText":"c", "newText":"d"})).unwrap();
        assert_eq!(args.edits.len(), 2);
        assert_eq!(args.edits[1].old_text, "c");
        assert!(
            parse_tool_arguments::<EditArgs>(json!({"path":"f", "edits":[]}))
                .unwrap()
                .edits
                .is_empty()
        );
    }

    #[test]
    fn invalid_typed_arguments_are_rejected() {
        for arguments in [
            json!({}),
            json!(null),
            json!([]),
            json!({"path":2, "edits":[]}),
            json!({"path":"f"}),
            json!({"path":"f", "edits":null}),
            json!({"path":"f", "edits":"not JSON"}),
            json!({"path":"f", "edits":[{"oldText":1,"newText":"b"}]}),
            json!({"path":"f", "edits":[{"oldText":"a"}]}),
            json!({"path":"f", "edits":{"oldText":"a"}}),
        ] {
            assert!(parse_tool_arguments::<EditArgs>(arguments).is_err());
        }
    }

    #[test]
    fn matches_all_edits_against_original_and_orders_by_position() {
        assert_eq!(
            apply("one two", &[("two", "one"), ("one", "two")]).unwrap(),
            "two one"
        );
        assert!(
            apply("one", &[("one", "two"), ("two", "three")])
                .unwrap_err()
                .contains("Could not find edits[1]")
        );
        assert_eq!(apply("abcd", &[("cd", "X"), ("ab", "Y")]).unwrap(), "YX");
        assert_eq!(
            apply("a😀bé", &[("😀", "Ω"), ("é", "終")]).unwrap(),
            "aΩb終"
        );
    }

    #[test]
    fn duplicate_needles_include_fuzzy_equivalents_and_nonoverlapping_counts() {
        assert_eq!(
            apply("a a a", &[("a", "b")]).unwrap_err(),
            "Found 3 occurrences of the text in file. The text must be unique. Please provide more context to make it unique."
        );
        assert!(
            apply("‘x’ 'x'", &[("'x'", "y")])
                .unwrap_err()
                .contains("Found 2 occurrences")
        );
        assert!(
            apply("a a b", &[("b", "c"), ("a", "d")])
                .unwrap_err()
                .contains("occurrences of edits[1]")
        );
        assert_eq!(apply("aaa", &[("aa", "z")]).unwrap(), "za");
    }

    #[test]
    fn overlapping_nested_and_duplicate_edits_fail_but_adjacent_edits_work() {
        for pairs in [
            vec![("abc", "x"), ("bc", "y")],
            vec![("abc", "x"), ("abc", "y")],
            vec![("ab", "x"), ("bc", "y")],
        ] {
            assert!(
                apply("abcd", &pairs)
                    .unwrap_err()
                    .contains("edits[0] and edits[1] overlap in file.")
            );
        }
        assert!(
            apply("abcd", &[("cd", "x"), ("bcd", "y")])
                .unwrap_err()
                .contains("edits[1] and edits[0] overlap")
        );
        assert_eq!(apply("abcd", &[("ab", "x"), ("cd", "y")]).unwrap(), "xy");
    }

    #[test]
    fn no_op_empty_and_not_found_errors_are_exact() {
        assert_eq!(
            apply("a", &[("a", "a")]).unwrap_err(),
            "No changes made to file. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        );
        assert_eq!(
            apply("ab", &[("a", "a"), ("b", "b")]).unwrap_err(),
            "No changes made to file. The replacements produced identical content."
        );
        assert_eq!(
            apply("a", &[("", "b")]).unwrap_err(),
            "oldText must not be empty in file."
        );
        assert_eq!(
            apply("a", &[("a", "b"), ("", "c")]).unwrap_err(),
            "edits[1].oldText must not be empty in file."
        );
        assert_eq!(
            apply("a", &[("b", "c")]).unwrap_err(),
            "Could not find the exact text in file. The old text must match exactly including all whitespace and newlines."
        );
        assert_eq!(
            apply("a\r\n", &[("a\n", "a\r\n")]).unwrap_err(),
            "No changes made to file. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        );
    }

    #[test]
    fn bom_and_first_line_ending_are_restored() {
        assert_eq!(
            apply("\u{feff}a\r\nb\r\n", &[("a\n", "x\ny\n")]).unwrap(),
            "\u{feff}x\r\ny\r\nb\r\n"
        );
        assert_eq!(apply("a\nb\r\nc\r", &[("b", "B")]).unwrap(), "a\nB\nc\n");
        assert_eq!(apply("a\r\nb\nc", &[("a", "A")]).unwrap(), "A\r\nb\r\nc");
        assert_eq!(apply("a\rb", &[("a\nb", "x\ry")]).unwrap(), "x\ny");
        assert_eq!(
            apply("\u{feff}\u{feff}a", &[("a", "b")]).unwrap(),
            "\u{feff}\u{feff}b"
        );
    }

    #[test]
    fn fuzzy_unicode_nfkc_quotes_dashes_and_spaces() {
        assert_eq!(
            normalize_for_fuzzy_match("Ａ ﬃ e\u{301} ‘x’ “y” —\u{00a0}z \t\u{feff}\n"),
            "A ffi é 'x' \"y\" - z\n"
        );
        assert_eq!(
            apply("Ａ ﬃ e\u{301}\n", &[("A ffi é", "ok")]).unwrap(),
            "ok\n"
        );
        assert_eq!(
            apply("‘hello’—world\u{00a0}!  \n", &[("'hello'-world !", "yes")]).unwrap(),
            "yes\n"
        );
        assert_eq!(normalize_for_fuzzy_match("x\u{0085}"), "x\u{0085}");
    }

    #[test]
    fn exact_matching_keeps_touched_line_bytes_and_fuzzy_preserves_untouched_lines() {
        assert_eq!(
            apply("‘keep’  x\t\n", &[("x", "y")]).unwrap(),
            "‘keep’  y\t\n"
        );
        assert_eq!(
            apply("‘keep’  \n‘change’  \n‘tail’\t\n", &[("'change'", "new")]).unwrap(),
            "‘keep’  \nnew\n‘tail’\t\n"
        );
        assert_eq!(
            apply("‘a’ x ‘b’  \nkeep\t", &[("'a'", "A"), ("‘b’", "B")]).unwrap(),
            "A x B\nkeep\t"
        );
        // Exact and fuzzy edits share the normalized coordinate space.
        assert_eq!(
            apply("Ａ first\n‘second’\n", &[("first", "1"), ("'second'", "2")]).unwrap(),
            "A 1\n2\n"
        );
    }

    #[test]
    fn fuzzy_multiline_and_duplicate_line_preservation() {
        assert_eq!(
            apply(
                "same  \n‘target’  \nsame\t\n",
                &[("'target'\nsame", "changed")]
            )
            .unwrap(),
            "same  \nchanged\n"
        );
        assert_eq!(
            apply("‘x’  \n‘x’\t\nunique ‘x’\n", &[("unique 'x'", "changed")]).unwrap(),
            "‘x’  \n‘x’\t\nchanged\n"
        );
        assert_eq!(
            apply("head\n‘x’\n‘tail’  \n", &[("'x'\n", "new\n")]).unwrap(),
            "head\nnew\n‘tail’  \n"
        );
        assert_eq!(apply("‘x’", &[("'x'", "end")]).unwrap(), "end");
    }

    #[test]
    fn node_style_lossy_utf8_and_fuzzy_empty_needle_edges() {
        assert_eq!(
            edit_bytes(&[0xff, b'x'], &replacements(&[("x", "y")]), "file").unwrap(),
            "�y"
        );
        assert_eq!(apply("x", &[(" ", "y")]).unwrap(), "yx");
        assert_eq!(
            apply("", &[(" ", "y")]).unwrap_err(),
            "Replacement range is outside the base content."
        );
        assert!(
            apply("abcd", &[(" ", "y")])
                .unwrap_err()
                .contains("Found 3 occurrences")
        );
    }
}
