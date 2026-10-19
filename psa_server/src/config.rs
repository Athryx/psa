use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

const DEFAULT_CONFIG: &str =
    "[server]\nlisten = \"127.0.0.1:8080\"\n\n[database]\npath = \"psa.sqlite3\"\n";

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    server: ServerConfig,
    database: DatabaseConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseConfig {
    path: PathBuf,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub database_path: PathBuf,
    pub listen: SocketAddr,
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
}

impl StartOptions {
    pub async fn resolve(self) -> Result<ResolvedConfig, ConfigError> {
        let data_dir = match self.data_dir {
            Some(path) => path,
            None => directories::ProjectDirs::from("", "", "psa")
                .ok_or(ConfigError::MissingDataDirectory)?
                .data_local_dir()
                .to_path_buf(),
        };

        tokio::fs::create_dir_all(&data_dir)
            .await
            .map_err(|source| ConfigError::Io {
                path: data_dir.clone(),
                source,
            })?;

        let explicit = self.config.is_some();
        let config_path = self.config.unwrap_or_else(|| data_dir.join("config.toml"));
        if !explicit {
            ensure_default_config(&config_path).await?;
        }

        let contents = tokio::fs::read_to_string(&config_path)
            .await
            .map_err(|source| ConfigError::Io {
                path: config_path.clone(),
                source,
            })?;
        let config: Config = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: config_path.clone(),
            source,
        })?;
        if config.database.path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyDatabasePath);
        }

        let database_path = data_dir.join(config.database.path);
        Ok(ResolvedConfig {
            data_dir,
            config_path,
            database_path,
            listen: self.listen.unwrap_or(config.server.listen),
        })
    }
}

async fn ensure_default_config(path: &Path) -> Result<(), ConfigError> {
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };

    file.write_all(DEFAULT_CONFIG.as_bytes())
        .await
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    file.sync_all().await.map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;

    Ok(())
}
