//! Probe discovery and the versioned server-to-probe protocol.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use time::OffsetDateTime;
/// UUID type used for persistent probe identities.
pub use uuid::Uuid;

/// Supported version of the server-to-probe WebSocket protocol.
pub const PROTOCOL_VERSION: u32 = 3;
/// Maximum payload bytes in a binary data frame, excluding its nine-byte header.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Maximum number of concurrent operations on one probe connection.
pub const MAX_OPERATIONS: usize = 128;

/// A server URL a probe cannot connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("server URL must be an HTTP(S) or WS(S) URL without credentials, query, or fragment")]
pub struct InvalidServerUrl;

/// Build the probe WebSocket connection URL for a server URL, preserving any path prefix.
///
/// Accepts HTTP(S) or WS(S), converts to WS(S), and appends the probe connect path.
/// Rejects malformed URLs, unsupported schemes, credentials, missing hosts,
/// queries, and fragments with [`InvalidServerUrl`].
pub fn parse_connect_url(value: &str) -> Result<String, InvalidServerUrl> {
    let mut url = reqwest::Url::parse(value).map_err(|_| InvalidServerUrl)?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(InvalidServerUrl);
    }
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err(InvalidServerUrl),
    };
    url.set_scheme(scheme).map_err(|_| InvalidServerUrl)?;
    // Accept an already-resolved connect URL too, as written for sandbox probes.
    let base = url.path().trim_end_matches('/');
    if !base.ends_with(crate::api::routes::PROBES_CONNECT) {
        url.set_path(&format!("{base}{}", crate::api::routes::PROBES_CONNECT));
    }
    Ok(url.into())
}

/// Persistent identity of a probe, retained across reconnections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeId(pub Uuid);

impl std::fmt::Display for ProbeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity and environment information advertised by a connecting probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeMetadata {
    /// Persistent probe identifier.
    pub id: ProbeId,
    /// Nonempty hostname, at most 255 bytes.
    pub hostname: String,
    /// Nonempty operating-system name, at most 64 bytes.
    pub os: String,
    /// Nonempty CPU architecture name, at most 64 bytes.
    pub architecture: String,
    /// Optional display label, at most 1024 bytes.
    pub label: Option<String>,
    /// Absolute UTF-8 probe data directory, at most 4096 bytes.
    pub data_dir: PathBuf,
    /// Optional absolute UTF-8 home directory, at most 4096 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<String>,
    /// Whether this probe shares the server's filesystem (advertised from config.toml).
    /// Host probes use server-owned skills in place instead of synchronizing copies.
    #[serde(default, deserialize_with = "deserialize_query_bool")]
    pub host: bool,
    /// Whether this probe can create and manage Docker sandboxes on its machine.
    #[serde(default, deserialize_with = "deserialize_query_bool")]
    pub sandboxes: bool,
}

// Flattened URL query metadata arrives as strings, unlike stored/API JSON.
fn deserialize_query_bool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Boolean(bool),
        Query(String),
    }

    match Flag::deserialize(deserializer)? {
        Flag::Boolean(value) => Ok(value),
        Flag::Query(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

impl ProbeMetadata {
    /// Check metadata length limits and require absolute UTF-8 directory paths.
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
            || self.home_dir.as_ref().is_some_and(|home| {
                home.len() > 4096
                    || home.contains('\0')
                    || !std::path::Path::new(home).is_absolute()
            })
        {
            return Err("invalid probe metadata".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn metadata() -> ProbeMetadata {
        ProbeMetadata {
            id: ProbeId(Uuid::nil()),
            hostname: "test".into(),
            os: "linux".into(),
            architecture: "x86_64".into(),
            label: None,
            data_dir: "/data".into(),
            home_dir: None,
            host: false,
            sandboxes: false,
        }
    }

    #[test]
    fn sandbox_flag_parses_from_query_strings_and_defaults_off() {
        let mut value = serde_json::to_value(metadata()).unwrap();
        value["sandboxes"] = serde_json::json!("true");
        assert!(
            serde_json::from_value::<ProbeMetadata>(value.clone())
                .unwrap()
                .sandboxes
        );
        value.as_object_mut().unwrap().remove("sandboxes");
        assert!(
            !serde_json::from_value::<ProbeMetadata>(value)
                .unwrap()
                .sandboxes
        );
    }

    #[test]
    fn optional_home_dir_round_trips() {
        let mut value = serde_json::to_value(metadata()).unwrap();
        assert!(value.get("home_dir").is_none());
        assert!(
            serde_json::from_value::<ProbeMetadata>(value.clone())
                .unwrap()
                .home_dir
                .is_none()
        );
        value["home_dir"] = serde_json::json!("/home/user");
        let decoded: ProbeMetadata = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.home_dir.as_deref(), Some("/home/user"));
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        value["home_dir"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<ProbeMetadata>(value)
                .unwrap()
                .home_dir
                .is_none()
        );
    }

    #[test]
    fn home_dir_must_be_an_absolute_bounded_path_without_nul() {
        let mut metadata = metadata();
        for valid in ["/", "/home/a b", &format!("/{}", "a".repeat(4095))] {
            metadata.home_dir = Some(valid.into());
            assert!(metadata.validate().is_ok());
        }
        for invalid in ["", "relative", "/a\0b", &format!("/{}", "a".repeat(4096))] {
            metadata.home_dir = Some(invalid.into());
            assert!(metadata.validate().is_err());
        }
    }
}

/// Server-side probe record with current connection state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    /// Most recently advertised metadata, flattened into the wire object.
    #[serde(flatten)]
    pub metadata: ProbeMetadata,
    /// Whether the probe currently has an active connection.
    pub is_active: bool,
    /// Time the probe was first registered, serialized as RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Time of the most recent connection, serialized as RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub last_connected_at: OffsetDateTime,
}

/// Initial probe handshake sent when opening a server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectProbe {
    /// Protocol version; must match [`PROTOCOL_VERSION`].
    pub version: u32,
    /// Probe identity and environment, flattened into the wire object.
    #[serde(flatten)]
    pub metadata: ProbeMetadata,
}

/// Routing for one standard stream of a process running on the probe.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Stdio {
    /// Exchange stream data through the probe protocol; the default.
    #[default]
    Piped,
    /// Connect the stream to the null device.
    Null,
    /// Inherit the corresponding stream of the probe process.
    Inherit,
}

/// Process specification executed directly on the probe, without an implicit shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    /// Executable path or name resolved using the probe's command search path.
    pub program: String,
    /// Arguments passed to the executable, excluding its program name.
    pub args: Vec<String>,
    /// Absolute UTF-8 working directory on the probe.
    pub cwd: PathBuf,
    /// Environment overrides; `None` removes a variable. Probe tools are prepended to `PATH`.
    pub env: BTreeMap<String, Option<String>>,
    /// Clear inherited environment variables before applying overrides.
    pub env_clear: bool,
    /// Standard input routing; piped input uses protocol stream 0.
    pub stdin: Stdio,
    /// Standard output routing; piped output uses protocol stream 1.
    pub stdout: Stdio,
    /// Standard error routing; piped output uses protocol stream 2.
    pub stderr: Stdio,
}

impl Command {
    /// Construct a command with no arguments or overrides, inherited environment, and piped streams.
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

    /// Append one argument without shell parsing.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append arguments in iteration order without shell parsing.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory on the probe; execution requires an absolute UTF-8 path.
    pub fn current_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Remove a variable from the child environment before the probe adds its tools to `PATH`.
    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env.insert(key.into(), None);
        self
    }

    /// Disable environment inheritance; probe tools are still added to `PATH`.
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    /// Set standard input routing.
    pub fn stdin(mut self, value: Stdio) -> Self {
        self.stdin = value;
        self
    }

    /// Set standard output routing.
    pub fn stdout(mut self, value: Stdio) -> Self {
        self.stdout = value;
        self
    }

    /// Set standard error routing.
    pub fn stderr(mut self, value: Stdio) -> Self {
        self.stderr = value;
        self
    }

    /// Set an environment override before the probe prepends its tools to `PATH`.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), Some(value.into()));
        self
    }
}

/// Filesystem or process operation requested from a probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Read file contents as binary data on stream 1.
    ReadFile {
        /// Absolute UTF-8 source path on the probe.
        path: PathBuf,
    },
    /// Write stream 0 to a file, creating it if absent; finish input with stream EOF.
    WriteFile {
        /// Absolute UTF-8 destination path on the probe.
        path: PathBuf,
        /// Append when true; otherwise truncate existing contents.
        append: bool,
    },
    /// Return immediate directory entries sorted by name.
    ListDir {
        /// Absolute UTF-8 directory path on the probe.
        path: PathBuf,
    },
    /// Create a directory and any missing parent directories.
    CreateDir {
        /// Absolute UTF-8 directory path on the probe.
        path: PathBuf,
    },
    /// Extract a tar archive received on stream 0 into a new directory.
    UploadDir {
        /// Absolute UTF-8 destination path, which must not already exist.
        path: PathBuf,
    },
    /// Atomically publish a tar directory, replacing an existing directory if present.
    SyncDir {
        /// Absolute UTF-8 destination path for the archive received on stream 0.
        path: PathBuf,
    },
    /// Stream a directory as a tar archive on stream 1.
    DownloadDir {
        /// Absolute UTF-8 source directory on the probe.
        path: PathBuf,
    },
    /// Run a process with streamed I/O and process-group signal handling.
    Spawn {
        /// Executable, arguments, environment, working directory, and stream routing.
        command: Command,
    },
    /// Manage a Docker sandbox container on a probe advertising sandbox support.
    Sandbox {
        /// Sandbox lifecycle request.
        request: SandboxRequest,
    },
}

/// Lifecycle request for sandbox containers managed by a probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SandboxRequest {
    /// Create a stopped container running a nested probe.
    Create {
        /// Container and nested probe specification.
        spec: SandboxSpec,
    },
    /// Start the sandbox container for a nested probe.
    Start {
        /// Nested probe identifying the sandbox.
        probe_id: ProbeId,
    },
    /// Stop the sandbox container for a nested probe.
    Stop {
        /// Nested probe identifying the sandbox.
        probe_id: ProbeId,
    },
    /// Remove the sandbox container and its nested probe configuration.
    Remove {
        /// Nested probe identifying the sandbox.
        probe_id: ProbeId,
    },
    /// Report every sandbox container managed by the probe as [`Event::Containers`].
    Inspect,
}

/// Container specification for a new sandbox.
///
/// Mounts carry no seed in the request itself: each seed in `seeds` follows on
/// input stream 0, in order, and the server then closes the stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Identity assigned to the nested probe.
    pub probe_id: ProbeId,
    /// Sandbox name, used as the nested probe label and container name suffix.
    pub name: String,
    /// Docker image reference.
    pub image: String,
    /// Host directories bound into the container.
    pub mounts: Vec<crate::api::sandboxes::Mount>,
    /// Seed archives streamed on input stream 0 after the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<SeedTransfer>,
}

/// A mount seed archive streamed after a sandbox create request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedTransfer {
    /// Index of the mount in [`SandboxSpec::mounts`] the seed belongs to.
    pub mount: usize,
    /// Size of the archive's `tar_data` in bytes.
    pub size: u64,
    /// Compression of the streamed archive.
    pub compression: crate::api::ArchiveCompression,
}

/// Docker state of one sandbox container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStatus {
    /// Nested probe identifying the sandbox.
    pub probe_id: ProbeId,
    /// Docker container name.
    pub container_name: String,
    /// Whether the container is running.
    pub state: crate::api::sandboxes::SandboxState,
}

/// Metadata for one immediate child of a listed directory, without following symlinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    /// UTF-8 entry name relative to the listed directory.
    pub name: String,
    /// Entry type: `file`, `directory`, `symlink`, or `other`.
    pub kind: String,
    /// Filesystem metadata length in bytes; not recursive directory size.
    pub size: u64,
}

/// Termination details for the directly spawned process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code when the process exited normally.
    pub code: Option<i32>,
    /// Unix signal number when the process was terminated by a signal.
    pub signal: Option<i32>,
}

/// Operation events and input controls exchanged through the probe transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A process has been spawned on the probe.
    Started {
        /// Process identifier when available.
        pid: Option<u32>,
    },
    /// Result of a sandbox inspection.
    Containers {
        /// Sandbox containers managed by the probe.
        containers: Vec<ContainerStatus>,
    },
    /// Result of a directory listing.
    Entries {
        /// Immediate directory children sorted by name.
        entries: Vec<DirectoryEntry>,
    },
    /// No more data will be sent on the specified stream.
    Eof {
        /// Stream identifier: 0 for input, 1 for output, or 2 for error output.
        stream: u8,
    },
    /// Request that the probe signal the spawned process group.
    Signal {
        /// Unix signal number to deliver.
        signal: i32,
    },
    /// The operation finished without a transport or execution error.
    Complete {
        /// Process exit status for spawn operations; absent for filesystem operations.
        status: Option<ExitStatus>,
    },
    /// The operation failed.
    Error {
        /// Machine-readable failure category.
        code: String,
        /// Human-readable error details.
        message: String,
    },
    /// Local cancellation notification; cancellation is sent as [`Control::Cancel`] on the wire.
    Cancelled,
    /// Binary stream data, encoded in binary frames rather than JSON events.
    #[serde(skip)]
    Data {
        /// Stream identifier: 0 for input, 1 for output, or 2 for error output.
        stream: u8,
        /// Chunk payload, at most [`CHUNK_SIZE`] bytes per wire frame.
        bytes: Vec<u8>,
    },
}

/// JSON control frame multiplexing operations over a probe WebSocket connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// Start a new operation on the probe.
    Request {
        /// Nonzero operation identifier, unique among active operations on the connection.
        id: u64,
        /// Operation to execute remotely.
        operation: Operation,
    },
    /// Deliver an operation event or input control.
    Event {
        /// Identifier of the operation receiving the event.
        id: u64,
        /// Event payload; binary data is sent in separate binary frames.
        event: Event,
    },
    /// Permit one more binary data frame on a stream after consuming its previous frame.
    Credit {
        /// Identifier of the operation whose stream may resume.
        id: u64,
        /// Stream identifier: 0 for input, 1 for output, or 2 for error output.
        stream: u8,
    },
    /// Cancel a live operation and release its transport state.
    Cancel {
        /// Identifier of the operation to cancel.
        id: u64,
    },
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn probe_server_urls_append_the_connect_route_once() {
        for input in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "ws://127.0.0.1:8080/probes/connect",
        ] {
            assert_eq!(
                parse_connect_url(input).unwrap(),
                "ws://127.0.0.1:8080/probes/connect"
            );
        }
        assert_eq!(
            parse_connect_url("https://psa.example/prefix").unwrap(),
            "wss://psa.example/prefix/probes/connect"
        );
    }
}
