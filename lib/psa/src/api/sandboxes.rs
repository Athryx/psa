//! Docker sandboxes managed by probes advertising sandbox support.
//!
//! A sandbox is a container on a manager probe's machine running a nested probe.
//! It is identified by the nested probe's [`ProbeId`].
use crate::api::{Archive, ProbeId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Host directory bound into a sandbox container.
///
/// The manager probe prepares mounts before creating the container: it checks
/// or creates the host directory as `host` says, then extracts `seed` into it.
/// If any mount fails, the directories and seed files created for the sandbox
/// are removed again and the sandbox is not created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Absolute directory path on the manager probe's machine.
    pub host_path: PathBuf,
    /// Absolute path inside the container.
    pub container_path: PathBuf,
    /// Allow the container to modify the mounted directory.
    #[serde(default)]
    pub writable: bool,
    /// Whether the host directory must exist or is created.
    #[serde(default)]
    pub host: HostPolicy,
    /// Files and directories extracted into the host directory when the
    /// sandbox is created, never on later starts. Existing directories are
    /// merged; a seed file whose path already exists fails the creation. Not
    /// kept in the sandbox record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Archive>,
}

/// How a mount's host directory is obtained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPolicy {
    /// The directory must already exist.
    #[default]
    Existing,
    /// Nothing may exist at the path; the directory is created with its parents.
    Create,
    /// The directory is created with its parents if missing, else reused.
    CreateOrReuse,
}

/// Largest total size of the seed archives of one sandbox, as sent.
pub const MAX_SANDBOX_SEED_BYTES: usize = 256 * 1024 * 1024;

/// Largest uncompressed size of one mount's seed archive.
pub const MAX_SANDBOX_SEED_UNPACKED_BYTES: usize = 256 * 1024 * 1024;

/// Total compressed size of the seed archives in `mounts`.
pub fn sandbox_seed_bytes(mounts: &[Mount]) -> usize {
    mounts
        .iter()
        .filter_map(|mount| mount.seed.as_ref())
        .map(|seed| seed.tar_data.len())
        .sum()
}

/// Request to create a sandbox on a manager probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSandbox {
    /// Docker image reference; the server's default image when absent.
    #[serde(default)]
    pub image: Option<String>,
    /// Sandbox name, unique per manager; generated when absent.
    #[serde(default)]
    pub name: Option<String>,
    /// Host directories bound into the container.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Start the container after creating it.
    #[serde(default = "default_start")]
    pub start: bool,
}

fn default_start() -> bool {
    true
}

impl Default for CreateSandbox {
    fn default() -> Self {
        Self {
            image: None,
            name: None,
            mounts: Vec::new(),
            start: true,
        }
    }
}

/// Container state of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// The container is running.
    Running,
    /// The container exists but is not running.
    Stopped,
    /// The manager probe has no container for this sandbox.
    Missing,
    /// The manager probe is not connected, so the state cannot be queried.
    Unknown,
    /// The sandbox was deleted. Its record remains so its probe and sessions
    /// stay associated with the manager; it cannot be started again.
    Deleted,
}

/// Server record of a sandbox with its current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sandbox {
    /// Nested probe running inside the container; identifies the sandbox.
    pub probe_id: ProbeId,
    /// Probe managing the container.
    pub manager_id: ProbeId,
    /// Sandbox name, unique per manager.
    pub name: String,
    /// Docker image reference the container was created from.
    pub image: String,
    /// Host directories bound into the container.
    pub mounts: Vec<Mount>,
    /// Current container state.
    pub state: SandboxState,
    /// Whether the nested probe is currently connected.
    pub probe_active: bool,
    /// Time the sandbox was created, serialized as RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Whether a sandbox name is usable in container names and probe labels:
/// 1-64 ASCII letters, digits, `-`, `_` or `.`, not starting with `-` or `.`.
pub fn is_valid_sandbox_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Query parameters for listing sandboxes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSandboxes {
    /// Only list sandboxes managed by this probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_id: Option<ProbeId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_defaults_to_starting_with_server_image() {
        let request: CreateSandbox = serde_json::from_str("{}").unwrap();
        assert!(request.start);
        assert!(request.image.is_none());
        assert!(request.mounts.is_empty());
    }

    #[test]
    fn mounts_default_to_existing_directories_without_seed() {
        let mount: Mount =
            serde_json::from_str(r#"{"host_path":"/h","container_path":"/c"}"#).unwrap();
        assert_eq!(mount.host, HostPolicy::Existing);
        assert!(mount.seed.is_none());
        assert!(
            !serde_json::to_value(&mount)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("seed")
        );

        let mount: Mount = serde_json::from_str(
            r#"{"host_path":"/h","container_path":"/c","host":"create_or_reuse","seed":{"tar_data":""}}"#,
        )
        .unwrap();
        assert_eq!(mount.host, HostPolicy::CreateOrReuse);
        assert!(mount.seed.is_some());
    }

    #[test]
    fn create_request_rejects_unknown_fields() {
        assert!(serde_json::from_str::<CreateSandbox>(r#"{"tag":"x"}"#).is_err());
    }
}
