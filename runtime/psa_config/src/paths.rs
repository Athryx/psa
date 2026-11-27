//! Layout of the PSA data directory shared by the server, probe, and local clients.

use std::path::{Path, PathBuf};

/// Server configuration, also read by a host probe for its `host` marker.
pub const SERVER_CONFIG_FILE: &str = "config.toml";
/// PSA keys and provider credentials; private to the owning user.
pub const AUTH_FILE: &str = "auth.toml";
/// Standalone probe configuration.
pub const PROBE_CONFIG_FILE: &str = "probe.toml";
/// Global agent definition files, one `*.toml` per agent.
pub const AGENTS_DIR: &str = "agents";
/// Desktop client settings and workspace layout.
pub const DESKTOP_SETTINGS_FILE: &str = "desktop.toml";
/// Terminal client settings.
pub const TUI_SETTINGS_FILE: &str = "tui.toml";

/// Held by a running server for its whole lifetime, so only one server uses a
/// data directory and stale control sockets can be removed safely.
pub const SERVER_LIFETIME_LOCK: &str = "server.lifetime.lock";
/// Held by local clients while they start, stop, or restart the server.
pub const SERVER_OPERATION_LOCK: &str = "server.operation.lock";
/// Private (0700) directory holding the server's local control socket.
pub const SERVER_CONTROL_DIR: &str = "server.control";
/// The local control socket inside [`SERVER_CONTROL_DIR`].
pub const SERVER_CONTROL_SOCKET: &str = "socket";
/// Logs of processes started by local clients.
pub const LOGS_DIR: &str = "logs";
/// Output of a server started by a local client, inside [`LOGS_DIR`].
pub const SERVER_LOG_FILE: &str = "server.log";

/// The platform data directory used when `--data-dir` is not given.
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "psa").map(|dirs| dirs.data_local_dir().to_owned())
}

/// Path of the server's local control socket in `data_dir`.
pub fn server_control_socket(data_dir: &Path) -> PathBuf {
    data_dir
        .join(SERVER_CONTROL_DIR)
        .join(SERVER_CONTROL_SOCKET)
}

/// Path of the log written by a client-started server in `data_dir`.
pub fn server_log_file(data_dir: &Path) -> PathBuf {
    data_dir.join(LOGS_DIR).join(SERVER_LOG_FILE)
}
