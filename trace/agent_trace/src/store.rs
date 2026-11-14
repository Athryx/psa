//! Directory layout: `session.json`, `agents/<id>.jsonl`, and `blobs/<hash>.<ext>`.
//!
//! Serialization is separate from filesystem access so callers without local disk
//! access (such as a remote probe) can write the same files themselves.

use crate::model::{AgentTrace, Event, FORMAT_VERSION, SessionMeta, SessionTrace};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};

pub const SESSION_FILE: &str = "session.json";
pub const AGENTS_DIR: &str = "agents";
pub const BLOBS_DIR: &str = "blobs";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}:{line}: {source}")]
    Json {
        path: String,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "unsupported trace format version {0} (expected {FORMAT_VERSION}; re-import or re-export the trace)"
    )]
    UnsupportedVersion(u32),
    #[error("{0} already contains a trace; choose another directory or overwrite")]
    AlreadyExists(PathBuf),
    #[error("invalid agent file path {0:?}")]
    InvalidAgentFile(String),
}

/// One file of a serialized trace, with a path relative to the session directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFile {
    pub path: String,
    pub contents: Vec<u8>,
}

impl SessionTrace {
    /// Assign unique, filesystem-safe agent log paths in the metadata.
    pub fn assign_agent_files(&mut self) {
        let mut used = HashSet::new();
        for agent in &mut self.agents {
            let stem = sanitize_file_stem(&agent.meta.id);
            let mut path = format!("{AGENTS_DIR}/{stem}.jsonl");
            let mut suffix = 2;
            while !used.insert(path.clone()) {
                path = format!("{AGENTS_DIR}/{stem}-{suffix}.jsonl");
                suffix += 1;
            }
            agent.meta.file = path;
        }
        self.meta.agents = self.agents.iter().map(|agent| agent.meta.clone()).collect();
    }

    /// Serialize every file of the trace, assigning agent file paths first.
    pub fn to_files(&mut self) -> Vec<TraceFile> {
        self.assign_agent_files();

        let mut files = Vec::with_capacity(1 + self.agents.len() + self.blobs.len());
        let mut session = serde_json::to_vec_pretty(&self.meta).expect("metadata serializes");
        session.push(b'\n');
        files.push(TraceFile {
            path: SESSION_FILE.into(),
            contents: session,
        });

        for agent in &self.agents {
            files.push(TraceFile {
                path: agent.meta.file.clone(),
                contents: serialize_events(&agent.events),
            });
        }

        for (name, contents) in &self.blobs {
            files.push(TraceFile {
                path: format!("{BLOBS_DIR}/{name}"),
                contents: contents.clone(),
            });
        }
        files
    }

    /// Write the trace into `dir`, refusing to replace an existing trace unless `overwrite`.
    pub async fn save(&mut self, dir: &Path, overwrite: bool) -> Result<(), StoreError> {
        let session_path = dir.join(SESSION_FILE);
        if !overwrite && tokio::fs::try_exists(&session_path).await.unwrap_or(false) {
            return Err(StoreError::AlreadyExists(dir.to_owned()));
        }

        for file in self.to_files() {
            let path = dir.join(&file.path);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|source| io_error(parent, source))?;
            }
            tokio::fs::write(&path, &file.contents)
                .await
                .map_err(|source| io_error(&path, source))?;
        }
        Ok(())
    }

    /// Read a trace's metadata and events. Blobs stay on disk; see [`Self::load_blobs`].
    pub async fn load(dir: &Path) -> Result<Self, StoreError> {
        let session_path = dir.join(SESSION_FILE);
        let bytes = read(&session_path).await?;
        let meta = parse_session_meta(&bytes, &session_path.display().to_string())?;

        let mut agents = Vec::with_capacity(meta.agents.len());
        for agent_meta in &meta.agents {
            let relative = validate_relative_path(&agent_meta.file)?;
            let path = dir.join(relative);
            let bytes = read(&path).await?;
            agents.push(AgentTrace {
                meta: agent_meta.clone(),
                events: parse_events(&bytes, &path.display().to_string())?,
            });
        }

        Ok(Self::from_parts(meta, agents))
    }

    /// Read every blob referenced from `blobs/` into memory.
    pub async fn load_blobs(&mut self, dir: &Path) -> Result<(), StoreError> {
        let blobs_dir = dir.join(BLOBS_DIR);
        let mut entries = match tokio::fs::read_dir(&blobs_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(&blobs_dir, source)),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error(&blobs_dir, source))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = read(&entry.path()).await?;
            self.blobs.insert(name, bytes);
        }
        Ok(())
    }
}

/// Encode events as JSON lines.
pub fn serialize_events(events: &[Event]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        serde_json::to_writer(&mut out, event).expect("events serialize");
        out.push(b'\n');
    }
    out
}

pub fn parse_session_meta(bytes: &[u8], path: &str) -> Result<SessionMeta, StoreError> {
    let meta: SessionMeta = serde_json::from_slice(bytes).map_err(|source| StoreError::Json {
        path: path.to_owned(),
        line: 0,
        source,
    })?;
    if meta.format_version != FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion(meta.format_version));
    }
    Ok(meta)
}

/// Parse JSON lines, skipping blank lines.
pub fn parse_events(bytes: &[u8], path: &str) -> Result<Vec<Event>, StoreError> {
    let mut events = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event = serde_json::from_slice(line).map_err(|source| StoreError::Json {
            path: path.to_owned(),
            line: index + 1,
            source,
        })?;
        events.push(event);
    }
    Ok(events)
}

/// Whether `dir` holds a trace.
pub async fn is_trace_dir(dir: &Path) -> bool {
    tokio::fs::try_exists(dir.join(SESSION_FILE))
        .await
        .unwrap_or(false)
}

fn sanitize_file_stem(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_start_matches('.');
    if stem.is_empty() {
        "agent".into()
    } else {
        stem.chars().take(120).collect()
    }
}

/// Reject absolute paths and parent components so metadata cannot escape the trace.
fn validate_relative_path(path: &str) -> Result<&Path, StoreError> {
    let relative = Path::new(path);
    let safe = !path.is_empty()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
    if safe {
        Ok(relative)
    } else {
        Err(StoreError::InvalidAgentFile(path.to_owned()))
    }
}

async fn read(path: &Path) -> Result<Vec<u8>, StoreError> {
    tokio::fs::read(path)
        .await
        .map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentMeta, EventKind, Source};

    fn sample() -> SessionTrace {
        let mut trace = SessionTrace::new("s1", Source::default_for("test"));
        let image = trace.add_image("image/png", vec![9, 9]);
        for id in ["main", "a/b", "a_b"] {
            trace.agents.push(AgentTrace {
                meta: AgentMeta::new(id),
                events: vec![Event::new(
                    None,
                    EventKind::User {
                        text: format!("hello {id}"),
                        images: vec![image.clone()],
                    },
                )],
            });
        }
        trace.update_event_ids();
        trace
    }

    #[test]
    fn agent_files_are_unique_and_safe() {
        let mut trace = sample();
        trace.assign_agent_files();
        let files: Vec<_> = trace.agents.iter().map(|a| a.meta.file.as_str()).collect();
        assert_eq!(
            files,
            [
                "agents/main.jsonl",
                "agents/a_b.jsonl",
                "agents/a_b-2.jsonl"
            ]
        );
        assert!(validate_relative_path("../x").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut trace = sample();
        trace.save(dir.path(), false).await.unwrap();
        assert!(matches!(
            trace.save(dir.path(), false).await,
            Err(StoreError::AlreadyExists(_))
        ));

        let mut loaded = SessionTrace::load(dir.path()).await.unwrap();
        assert!(loaded.blobs.is_empty());
        loaded.load_blobs(dir.path()).await.unwrap();
        assert_eq!(loaded, trace);
    }
}
