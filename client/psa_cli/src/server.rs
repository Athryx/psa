//! `psa server`: manage the server of a data directory through its local
//! control socket, or run one in the foreground.
use crate::{CliError, ExecutableOptions, launch};
use client_shared::{
    ConnectionSettings,
    executables::{Executable, resolve_executable},
    local::{LocalServerManager, LocalServerStatus},
};
use std::{ffi::OsString, net::SocketAddr, path::PathBuf, process::ExitCode};

/// Exit status of `psa server status` for a stopped server (the LSB convention).
const STATUS_STOPPED: u8 = 3;

#[derive(Debug, clap::Subcommand)]
pub enum ServerCommand {
    /// Start the server in the background; logs go to <data-dir>/logs/server.log.
    Start {
        /// Listen address instead of the one in config.toml.
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Stop the running server.
    Stop,
    /// Stop the server if it runs, then start it in the background.
    Restart {
        /// Listen address instead of the one in config.toml.
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Show whether the server runs. Exits with 3 when it is stopped.
    Status,
    /// Run the server in the foreground with logs on the terminal.
    Run {
        /// Extra arguments for `psa_server start`, such as `--listen` or `--config`.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
}

pub async fn run(
    command: ServerCommand,
    data_dir: PathBuf,
    executables: ExecutableOptions,
) -> Result<ExitCode, CliError> {
    let server = resolve_executable(Executable::Server, executables.server_binary).await?;
    // Sandboxes need a probe executable to mount. Without one the server only
    // fails if probe.toml enables sandboxes, so a missing probe is not an error.
    let probe = resolve_executable(Executable::Probe, executables.probe_binary)
        .await
        .ok();

    match command {
        ServerCommand::Start { listen } => {
            let manager = build_manager(server, data_dir, probe, listen);
            let status = manager.start().await?;
            print_status(&manager, &status);
            Ok(ExitCode::SUCCESS)
        }
        ServerCommand::Stop => {
            let manager = build_manager(server, data_dir, probe, None);
            manager.stop().await?;
            println!("stopped");
            Ok(ExitCode::SUCCESS)
        }
        ServerCommand::Restart { listen } => {
            let manager = build_manager(server, data_dir, probe, listen);
            let status = manager.restart().await?;
            print_status(&manager, &status);
            Ok(ExitCode::SUCCESS)
        }
        ServerCommand::Status => {
            let manager = build_manager(server, data_dir, probe, None);
            let status = manager.status().await?;
            print_status(&manager, &status);
            Ok(match status {
                LocalServerStatus::Running { .. } => ExitCode::SUCCESS,
                LocalServerStatus::Stopped => ExitCode::from(STATUS_STOPPED),
            })
        }
        ServerCommand::Run { args } => {
            let mut command = tokio::process::Command::new(server);
            command.arg("start").arg("--data-dir").arg(&data_dir);
            if let Some(probe) = probe {
                command.arg("--probe-binary").arg(probe);
            }
            command.args(args);
            launch::run_foreground(command).await
        }
    }
}

pub(crate) fn build_manager(
    server: PathBuf,
    data_dir: PathBuf,
    probe: Option<PathBuf>,
    listen: Option<SocketAddr>,
) -> LocalServerManager {
    LocalServerManager::new(server, data_dir)
        .with_probe_binary(probe)
        .with_listen(listen)
}

/// Print the status without the client key, which stays in the control socket
/// and `auth.toml`.
fn print_status(manager: &LocalServerManager, status: &LocalServerStatus) {
    match status {
        LocalServerStatus::Running {
            connection: ConnectionSettings { api_url, .. },
        } => {
            println!("running");
            println!("  url:      {api_url}");
        }
        LocalServerStatus::Stopped => println!("stopped"),
    }
    println!("  data dir: {}", manager.data_dir().display());
    println!("  log:      {}", manager.log_path().display());
}
