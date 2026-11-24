//! Layout of the PSA data directory shared by the server, probe, and desktop client.

use std::path::PathBuf;

/// Server configuration, also read by a host probe for its `host` marker.
pub const SERVER_CONFIG_FILE: &str = "config.toml";
/// PSA keys and provider credentials; private to the owning user.
pub const AUTH_FILE: &str = "auth.toml";
/// Standalone probe configuration.
pub const PROBE_CONFIG_FILE: &str = "probe.toml";
/// Global agent definition files, one `*.toml` per agent.
pub const AGENTS_DIR: &str = "agents";

/// The platform data directory used when `--data-dir` is not given.
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "psa").map(|dirs| dirs.data_local_dir().to_owned())
}
