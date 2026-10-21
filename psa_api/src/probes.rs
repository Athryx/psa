//! Probe discovery and the versioned server-to-probe protocol.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use time::OffsetDateTime;
pub use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
pub const CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_OPERATIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeId(pub Uuid);

impl std::fmt::Display for ProbeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeMetadata {
    pub id: ProbeId,
    pub hostname: String,
    pub os: String,
    pub architecture: String,
    pub label: Option<String>,
    pub data_dir: PathBuf,
}

impl ProbeMetadata {
    pub fn validate(&self) -> Result<(), String> {
        if self.hostname.is_empty()
            || self.hostname.len() > 255
            || self.os.is_empty()
            || self.os.len() > 64
            || self.architecture.is_empty()
            || self.architecture.len() > 64
            || self.label.as_ref().is_some_and(|s| s.len() > 1024)
            || !self.data_dir.is_absolute()
            || self.data_dir.to_str().is_none()
            || self.data_dir.as_os_str().len() > 4096
        {
            return Err("invalid probe metadata".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    #[serde(flatten)]
    pub metadata: ProbeMetadata,
    pub is_active: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_connected_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectProbe {
    pub version: u32,
    #[serde(flatten)]
    pub metadata: ProbeMetadata,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Stdio {
    #[default]
    Piped,
    Null,
    Inherit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, Option<String>>,
    pub env_clear: bool,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

impl Command {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: BTreeMap::new(),
            env_clear: false,
            stdin: Stdio::Piped,
            stdout: Stdio::Piped,
            stderr: Stdio::Piped,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn current_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env.insert(key.into(), None);
        self
    }

    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    pub fn stdin(mut self, value: Stdio) -> Self {
        self.stdin = value;
        self
    }

    pub fn stdout(mut self, value: Stdio) -> Self {
        self.stdout = value;
        self
    }

    pub fn stderr(mut self, value: Stdio) -> Self {
        self.stderr = value;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), Some(value.into()));
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    ReadFile { path: PathBuf },
    WriteFile { path: PathBuf, append: bool },
    ListDir { path: PathBuf },
    CreateDir { path: PathBuf },
    UploadDir { path: PathBuf },
    DownloadDir { path: PathBuf },
    Spawn { command: Command },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        pid: Option<u32>,
    },
    Entries {
        entries: Vec<DirectoryEntry>,
    },
    Eof {
        stream: u8,
    },
    Signal {
        signal: i32,
    },
    Complete {
        status: Option<ExitStatus>,
    },
    Error {
        code: String,
        message: String,
    },
    Cancelled,
    #[serde(skip)]
    Data {
        stream: u8,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Request { id: u64, operation: Operation },
    Event { id: u64, event: Event },
    Credit { id: u64, stream: u8 },
    Cancel { id: u64 },
}
