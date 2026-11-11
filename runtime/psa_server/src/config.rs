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
default_agent_name = "root"

[server]
listen = "127.0.0.1:8080"
autostart_local_probe = true

[database]
path = "psa.sqlite3"

# Used when session creation omits model_config.
[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"

# Fallbacks for omitted fields in agent files and API registrations.
# Explicit empty arrays override these defaults; hook stages inherit separately.
[agent_defaults]
bundles = [{ FileTools = { read_only = false } }, "Bash", "Subagents", { Goal = { interactive_only = true } }, { Compaction = {} }]

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
session_update = ["SessionModelUpdate"]
before_request = []
after_request = []
"#;

/// Global agent files generated with a new configuration: a selectable default
/// agent whose only permitted subagent is a leaf worker using its parent's model.
const DEFAULT_AGENTS: [(&str, &str); 2] = [
    (
        "root.toml",
        r#"name = "root"
description = "General-purpose assistant"
selectable = true
models = ["Global"]
default = true

[subagents]
global_defaults = false
project_defaults = false
subagents = ["subagent"]
"#,
    ),
    (
        "subagent.toml",
        r#"name = "subagent"
description = "Delegated worker using its parent's model and effort"
models = ["Parent"]

# A leaf worker: it cannot create subagents of its own.
[subagents]
global_defaults = false
project_defaults = false
"#,
    ),
];

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
    #[serde(default = "default_agent_name")]
    default_agent_name: String,
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
    #[serde(default)]
    autostart_local_probe: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseConfig {
    path: PathBuf,
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
            ensure_default_config(&config_path, &data_dir).await?;
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
            default_agent_name: config.default_agent_name,
            data_dir,
            config_path,
            database_path,
            listen: self.listen.unwrap_or(config.server.listen),
            autostart_local_probe: config.server.autostart_local_probe,
            session_defaults: config.session_defaults,
            agent_defaults: config.agent_defaults,
        })
    }
}

fn default_agent_name() -> String {
    "root".into()
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
// Default agents are only written alongside a newly generated configuration.
async fn ensure_default_config(path: &Path, data_dir: &Path) -> Result<(), ConfigError> {
    if !write_new_file(path, DEFAULT_CONFIG).await? {
        return Ok(());
    }

    let agents = data_dir.join("agents");
    tokio::fs::create_dir_all(&agents)
        .await
        .map_err(|source| ConfigError::Io {
            path: agents.clone(),
            source,
        })?;
    for (name, contents) in DEFAULT_AGENTS {
        write_new_file(&agents.join(name), contents).await?;
    }
    Ok(())
}

/// Writes and syncs `contents` only if `path` does not exist yet.
/// Returns whether the file was created.
async fn write_new_file(path: &Path, contents: &str) -> Result<bool, ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(source) => return Err(io_error(source)),
    };

    file.write_all(contents.as_bytes())
        .await
        .map_err(io_error)?;
    file.sync_all().await.map_err(io_error)?;
    Ok(true)
}
