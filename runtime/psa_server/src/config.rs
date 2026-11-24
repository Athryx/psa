//! Server command line and startup resolution. The `config.toml` schema and
//! its defaults live in `psa_config::server`.

use clap::{Args, Parser, Subcommand};
use llm_api::providers::ModelConfig;
pub use psa_config::server::{AgentDefaults, default_model_config};
use psa_config::{
    PathIoError, paths,
    server::{self, ServerConfig},
};
use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf};

#[derive(Debug, Parser)]
#[command(about = "PSA session and event server", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Start(StartOptions),
}

#[derive(Debug, Args, Default)]
pub struct StartOptions {
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub listen: Option<SocketAddr>,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub host: bool,
    pub default_agent_name: String,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub database_path: PathBuf,
    pub listen: SocketAddr,
    pub autostart_local_probe: bool,
    pub session_defaults: ModelConfig,
    pub agent_defaults: AgentDefaults,
    pub default_sandbox_image: Option<String>,
    pub max_concurrent_decision_calls: NonZeroUsize,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine application data directory; specify --data-dir")]
    MissingDataDirectory,
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("database path must not be empty")]
    EmptyDatabasePath,
    #[error("invalid agent defaults: {0}")]
    AgentDefaults(#[from] crate::components::bundles::CompletionConfigError),
}

impl From<PathIoError> for ConfigError {
    fn from(error: PathIoError) -> Self {
        Self::Io {
            path: error.path,
            source: error.source,
        }
    }
}

impl StartOptions {
    pub async fn resolve(self) -> Result<ResolvedConfig, ConfigError> {
        let data_dir = match self.data_dir {
            Some(path) => path,
            None => paths::default_data_dir().ok_or(ConfigError::MissingDataDirectory)?,
        };

        tokio::fs::create_dir_all(&data_dir)
            .await
            .map_err(|source| ConfigError::Io {
                path: data_dir.clone(),
                source,
            })?;

        let explicit = self.config.is_some();
        let config_path = self
            .config
            .unwrap_or_else(|| data_dir.join(paths::SERVER_CONFIG_FILE));
        if !explicit {
            server::ensure_default_config(&config_path, &data_dir).await?;
        }

        let contents = tokio::fs::read_to_string(&config_path)
            .await
            .map_err(|source| ConfigError::Io {
                path: config_path.clone(),
                source,
            })?;
        let config = ServerConfig::parse(&contents).map_err(|source| ConfigError::Parse {
            path: config_path.clone(),
            source,
        })?;
        if config.database.path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyDatabasePath);
        }

        crate::components::bundles::validate_completion_config(&config.agent_defaults.bundles)?;

        let database_path = data_dir.join(config.database.path);
        Ok(ResolvedConfig {
            host: config.host,
            default_agent_name: config.default_agent_name,
            data_dir,
            config_path,
            database_path,
            listen: self.listen.unwrap_or(config.server.listen),
            autostart_local_probe: config.server.autostart_local_probe,
            session_defaults: config.session_defaults,
            agent_defaults: config.agent_defaults,
            default_sandbox_image: config
                .sandboxes
                .default_image
                .filter(|image| !image.trim().is_empty()),
            max_concurrent_decision_calls: config.decisions.max_concurrent_calls,
        })
    }
}
