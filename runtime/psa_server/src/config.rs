use clap::{Args, Parser, Subcommand};
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa_api::AgentDefaults;
use serde::{Deserialize, Deserializer};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

const DEFAULT_CONFIG: &str = r#"host = true

[server]
listen = "127.0.0.1:8080"

[database]
path = "psa.sqlite3"

# Used when session creation omits model_config.
[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"

# Fallbacks for omitted fields in agent files and API registrations.
# Explicit empty arrays override these defaults; hook stages inherit separately.
[agent_defaults]
bundles = [{ FileTools = { read_only = false } }, "Bash", "Subagents", { Goal = { interactive_only = true } }]

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
session_update = []
before_request = [{ Compaction = {} }]
after_request = []
"#;

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
    #[serde(default)]
    host: bool,
    server: ServerConfig,
    database: DatabaseConfig,
    #[serde(
        default = "default_model_config",
        deserialize_with = "deserialize_session_defaults"
    )]
    session_defaults: ModelConfig,
    #[serde(default)]
    agent_defaults: AgentDefaults,
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
    pub host: bool,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub database_path: PathBuf,
    pub listen: SocketAddr,
    pub session_defaults: ModelConfig,
    pub agent_defaults: AgentDefaults,
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
    AgentDefaults(#[from] psa_api::CompletionConfigurationError),
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

        psa_api::validate_completion_config(
            &config.agent_defaults.bundles,
            &config.agent_defaults.hooks,
        )?;

        let database_path = data_dir.join(config.database.path);
        Ok(ResolvedConfig {
            host: config.host,
            data_dir,
            config_path,
            database_path,
            listen: self.listen.unwrap_or(config.server.listen),
            session_defaults: config.session_defaults,
            agent_defaults: config.agent_defaults,
        })
    }
}

/// Built-in session fallback, also used when `[session_defaults]` is absent.
pub fn default_model_config() -> ModelConfig {
    ModelConfig {
        model: Model {
            provider: ProviderType::OpenAiCodex,
            api_name: "gpt-5".into(),
        },
        effort: Effort::Medium,
    }
}

fn deserialize_session_defaults<'de, D>(deserializer: D) -> Result<ModelConfig, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SessionDefaults {
        model: String,
        effort: Effort,
    }

    let defaults = SessionDefaults::deserialize(deserializer)?;
    let model = Model::parse_slug(&defaults.model).map_err(serde::de::Error::custom)?;
    Ok(ModelConfig {
        model,
        effort: defaults.effort,
    })
}

// Existing configurations are never rewritten; a missing host marker stays false.
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
