//! `psa`: the terminal client, and the entry point for running and managing
//! local PSA processes. Without a subcommand it opens the TUI. The server,
//! desktop client, and probe stay separate executables; this binary locates
//! and launches them.
mod launch;
mod server;
mod tui;

use clap::{Parser, Subcommand};
use client_shared::executables::{Executable, ExecutableNotFound, resolve_executable};
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(
    name = "psa",
    about = "PSA terminal client; run without a command to open it",
    version
)]
struct Cli {
    /// PSA data directory shared by the server, probe, and clients.
    #[arg(long, global = true, env = "PSA_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Server URL for the terminal client; without it, the local server of the
    /// data directory is used and started if needed.
    #[arg(long, env = "PSA_URL")]
    url: Option<String>,
    /// Client key for --url (`[psa].client_key` in the server's auth.toml).
    #[arg(long, env = "PSA_KEY", hide_env_values = true, requires = "url")]
    key: Option<String>,
    #[command(flatten)]
    executables: ExecutableOptions,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Paths of the launched executables. Each defaults to its environment
/// variable, then the executable beside `psa`, then `PATH`.
#[derive(Debug, clap::Args)]
struct ExecutableOptions {
    /// Server executable [fallback: $PSA_SERVER_BIN].
    #[arg(long, global = true)]
    server_binary: Option<PathBuf>,
    /// Desktop client executable [fallback: $PSA_DESKTOP_BIN].
    #[arg(long, global = true)]
    desktop_binary: Option<PathBuf>,
    /// Static probe executable [fallback: $PSA_PROBE_BIN].
    #[arg(long, global = true)]
    probe_binary: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the local server of the data directory.
    #[command(subcommand)]
    Server(server::ServerCommand),
    /// Launch the desktop client.
    Gui {
        /// Extra arguments for the desktop client.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run a probe that connects to a server.
    #[command(subcommand)]
    Probe(ProbeCommand),
}

#[derive(Debug, Subcommand)]
enum ProbeCommand {
    /// Run a probe in the foreground. A local server already runs its own
    /// probe, so use a separate data directory for an additional one.
    Run {
        /// Server URL, saved to probe.toml.
        #[arg(long)]
        server: Option<String>,
        /// Display label, saved to probe.toml.
        #[arg(long)]
        label: Option<String>,
        /// Extra arguments for `psa_probe start`.
        #[arg(last = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("cannot determine the PSA data directory; use --data-dir or PSA_DATA_DIR")]
    MissingDataDirectory,
    #[error(transparent)]
    ExecutableNotFound(#[from] ExecutableNotFound),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("--url needs --key or PSA_KEY")]
    MissingKey,
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("{0}")]
    Settings(String),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("psa: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let data_dir = resolve_data_dir(cli.data_dir)?;
    let executables = cli.executables;

    let Some(command) = cli.command else {
        let target = match cli.url {
            Some(api_url) => {
                let client_key = cli.key.ok_or(CliError::MissingKey)?;
                tui::Target::Remote(client_shared::ConnectionSettings {
                    api_url,
                    client_key,
                })
            }
            None => {
                let server = resolve_executable(Executable::Server, executables.server_binary)
                    .await
                    .unwrap_or_else(|_| PathBuf::from(Executable::Server.name()));
                let probe = resolve_executable(Executable::Probe, executables.probe_binary)
                    .await
                    .ok();
                tui::Target::Local(server::build_manager(server, data_dir.clone(), probe, None))
            }
        };
        tui::run(target, &data_dir).await?;
        return Ok(ExitCode::SUCCESS);
    };

    match command {
        Command::Server(command) => server::run(command, data_dir, executables).await,
        Command::Gui { args } => {
            let desktop =
                resolve_executable(Executable::Desktop, executables.desktop_binary).await?;
            // Pass the server we would use so the GUI manages the same one.
            // Without one, the desktop client reports the lookup failure itself.
            let server = resolve_executable(Executable::Server, executables.server_binary)
                .await
                .ok();

            let mut command = tokio::process::Command::new(desktop);
            command.arg("--data-dir").arg(&data_dir);
            if let Some(server) = server {
                command.arg("--server-binary").arg(server);
            }
            command.args(args);
            launch::run_foreground(command).await
        }
        Command::Probe(ProbeCommand::Run {
            server,
            label,
            args,
        }) => {
            let probe = resolve_executable(Executable::Probe, executables.probe_binary).await?;

            let mut command = tokio::process::Command::new(probe);
            command.arg("start").arg("--data-dir").arg(&data_dir);
            if let Some(server) = server {
                command.arg("--server").arg(server);
            }
            if let Some(label) = label {
                command.arg("--label").arg(label);
            }
            command.args(args);
            launch::run_foreground(command).await
        }
    }
}

/// Make the data directory absolute so messages and child processes agree on it.
fn resolve_data_dir(explicit: Option<PathBuf>) -> Result<PathBuf, CliError> {
    let data_dir = explicit
        .or_else(psa_config::paths::default_data_dir)
        .ok_or(CliError::MissingDataDirectory)?;
    Ok(std::path::absolute(data_dir)?)
}
