//! Importers from other harnesses' session logs.
//!
//! Importers are deliberately lossy: they follow the active branch, keep content the
//! model saw or produced, and drop UI state, signatures, and account details.

mod claude_code;
mod codex;
mod pi;

use crate::model::SessionTrace;
use base64::Engine;
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
    str::FromStr,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Pi,
}

impl Harness {
    pub const ALL: [Self; 3] = [Self::ClaudeCode, Self::Codex, Self::Pi];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    /// Default session directory under the user's home directory.
    pub fn default_root(self) -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(match self {
            Self::ClaudeCode => home.join(".claude/projects"),
            Self::Codex => home.join(".codex/sessions"),
            Self::Pi => home.join(".pi/agent/sessions"),
        })
    }
}

impl FromStr for Harness {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|harness| harness.as_str() == value)
            .ok_or_else(|| format!("unknown harness {value:?}; expected claude-code, codex, or pi"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Import the session whose main log is `path`. A subagent log imports its whole session.
pub async fn import_session(harness: Harness, path: &Path) -> Result<SessionTrace, ImportError> {
    let mut trace = match harness {
        Harness::ClaudeCode => claude_code::import(path).await?,
        Harness::Codex => codex::import(path).await?,
        Harness::Pi => pi::import(path).await?,
    };
    trace.register_used_tools();
    trace.update_time_bounds();
    trace.update_event_ids();
    Ok(trace)
}

/// Most recently modified top-level session log under the harness's default root.
pub async fn find_latest_session(harness: Harness) -> Result<PathBuf, ImportError> {
    let root = harness
        .default_root()
        .ok_or_else(|| ImportError::Invalid("HOME is not set".into()))?;
    let files = collect_jsonl_files(&root, 6).await?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for path in files {
        let is_subagent = path.components().any(|c| c.as_os_str() == "subagents");
        if is_subagent {
            continue;
        }
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, path));
        }
    }
    newest
        .map(|(_, path)| path)
        .ok_or_else(|| ImportError::Invalid(format!("no sessions under {}", root.display())))
}

/// Recursively list `*.jsonl` files, descending at most `max_depth` directories.
pub(crate) async fn collect_jsonl_files(
    root: &Path,
    max_depth: usize,
) -> Result<Vec<PathBuf>, ImportError> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_owned(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && depth > 0 => continue,
            Err(source) => return Err(ImportError::Io { path: dir, source }),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| ImportError::Io {
                path: dir.clone(),
                source,
            })?
        {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() && depth < max_depth {
                pending.push((path, depth + 1));
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Parse a JSON-lines file, skipping blank and malformed lines (such as a torn last write).
pub(crate) async fn read_jsonl(path: &Path) -> Result<Vec<Value>, ImportError> {
    let text = read_text(path).await?;
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// Parse only the first line of a JSON-lines file.
pub(crate) async fn read_first_record(path: &Path) -> Result<Option<Value>, ImportError> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|source| ImportError::Io {
            path: path.to_owned(),
            source,
        })?;
    let mut line = String::new();
    tokio::io::BufReader::new(file)
        .read_line(&mut line)
        .await
        .map_err(|source| ImportError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(serde_json::from_str(&line).ok())
}

pub(crate) async fn read_text(path: &Path) -> Result<String, ImportError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|source| ImportError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn parse_timestamp(value: Option<&Value>) -> Option<OffsetDateTime> {
    match value? {
        Value::String(text) => OffsetDateTime::parse(text, &Rfc3339).ok(),
        Value::Number(number) => {
            let millis = number.as_i64()?;
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
        }
        _ => None,
    }
}

pub(crate) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

pub(crate) fn string_field(value: &Value, key: &str) -> Option<String> {
    str_field(value, key).map(str::to_owned)
}

pub(crate) fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

/// Add a base64 image to the trace, ignoring undecodable data.
pub(crate) fn add_base64_image(
    trace: &mut SessionTrace,
    mime: &str,
    data: &str,
) -> Option<crate::model::ImageRef> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    Some(trace.add_image(mime, bytes))
}

/// Add an image from a `data:<mime>;base64,<data>` URL.
pub(crate) fn add_data_url_image(
    trace: &mut SessionTrace,
    url: &str,
) -> Option<crate::model::ImageRef> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    add_base64_image(trace, mime, data)
}

/// Parse JSON arguments, keeping the raw string when they are not valid JSON.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// Concatenate the text of content blocks with `text`-like fields, separated by newlines.
pub(crate) fn join_block_text<'a>(blocks: impl IntoIterator<Item = &'a Value>) -> String {
    let texts: Vec<&str> = blocks
        .into_iter()
        .filter_map(|block| str_field(block, "text"))
        .collect();
    texts.join("\n")
}

/// Assert that `event` is a tool call whose result has `expected` output.
#[cfg(test)]
pub(crate) fn assert_tool_output(event: &crate::model::Event, expected: &str) {
    let crate::model::EventKind::ToolCall {
        result: Some(result),
        ..
    } = &event.kind
    else {
        panic!("expected an answered tool call, got {event:?}");
    };
    assert_eq!(result.output, expected);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_string_and_millisecond_timestamps() {
        let from_string = parse_timestamp(Some(&json!("2026-11-08T17:05:37.554Z"))).unwrap();
        let from_millis = parse_timestamp(Some(&json!(1794600406453_i64))).unwrap();
        assert_eq!(from_string.year(), 2026);
        assert_eq!(from_millis.year(), 2026);
        assert!(parse_timestamp(Some(&json!(true))).is_none());
    }

    #[test]
    fn keeps_unparseable_arguments_as_strings() {
        assert_eq!(parse_arguments("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(parse_arguments("const x"), json!("const x"));
    }
}
