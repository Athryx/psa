use clap::{Args, Parser, Subcommand};
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa::api::{AgentConfig, AgentConfigInput, BundleConfig};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    net::SocketAddr,
    num::NonZeroUsize,
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

# Docker image for sandboxes created without an explicit image.
# [sandboxes]
# default_image = "ubuntu:24.04"

# Decision-model calls a session runs at once; further calls queue.
# [decisions]
# max_concurrent_calls = 8

# Used when session creation omits model_config.
[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"

# Fallback bundles for agent files and API registrations that omit them.
# An explicit list, even an empty one, replaces these defaults.
[agent_defaults]
bundles = [{ Startup = {} }, { FileTools = { read_only = false } }, "Bash", "Subagents", { Goal = { interactive_only = true } }, { Compaction = {} }]
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
first_model_default = true

[subagents]
global_defaults = false
group_defaults = false
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
group_defaults = false
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
    #[serde(default)]
    sandboxes: SandboxConfig,
    #[serde(default)]
    decisions: DecisionsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionsConfig {
    #[serde(default = "default_max_concurrent_decision_calls")]
    max_concurrent_calls: NonZeroUsize,
}

impl Default for DecisionsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_calls: default_max_concurrent_decision_calls(),
        }
    }
}

fn default_max_concurrent_decision_calls() -> NonZeroUsize {
    crate::components::bundles::decision_models::DEFAULT_MAX_CONCURRENT_CALLS
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxConfig {
    default_image: Option<String>,
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
        model: Model,
        effort: Effort,
    }

    let defaults = SessionDefaults::deserialize(deserializer)?;
    Ok(ModelConfig {
        model: defaults.model,
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

/// Server-configured fallbacks. Explicit definition lists replace these lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentDefaults {
    /// Ordered fallback bundle configurations; defaults to empty.
    pub bundles: Vec<BundleConfig>,
}

impl AgentDefaults {
    /// Fill an omitted bundle list from these defaults. An explicit list, even
    /// an empty one, replaces the defaults rather than merging with them.
    /// Models and dependencies are not validated here.
    pub fn resolve(&self, input: AgentConfigInput) -> AgentConfig {
        AgentConfig {
            config_id: input.config_id,
            description: input.description,
            selectable: input.selectable,
            bundles: input.bundles.unwrap_or_else(|| self.bundles.clone()),
            models: input.models,
            first_model_default: input.first_model_default,
            subagents: input.subagents,
            skills: input.skills,
        }
    }
}

#[cfg(test)]
mod agent_defaults_tests {
    use super::*;
    use psa::api::{ConfigGroup, ConfigId, ModelSelection};
    use serde_json::json;

    fn defaults() -> AgentDefaults {
        AgentDefaults {
            bundles: vec![
                BundleConfig::Startup {
                    base_prompt: true,
                    project_context: true,
                    cwd: true,
                    skills: true,
                },
                BundleConfig::FileTools { read_only: false },
                BundleConfig::StopWithoutTools,
            ],
        }
    }

    #[test]
    fn omitted_lists_inherit_and_explicit_empty_lists_disable_defaults() {
        let id = json!({"config_group": "Global", "name": "agent"});
        let input: AgentConfigInput =
            serde_json::from_value(json!({"config_id": id, "models": ["Global"]})).unwrap();
        let resolved = defaults().resolve(input.clone());
        assert_eq!(resolved.bundles, defaults().bundles);
        let wire = serde_json::to_value(&input).unwrap();
        assert!(wire.get("bundles").is_none());
        assert!(wire.get("tools").is_none());
        assert!(wire.get("hooks").is_none());
        let input: AgentConfigInput =
            serde_json::from_value(json!({"config_id": id, "models": ["Global"], "bundles": []}))
                .unwrap();
        assert!(defaults().resolve(input).bundles.is_empty());
        assert!(
            serde_json::from_value::<AgentConfigInput>(
                json!({"config_id": id, "models": ["Global"], "hooks": {}})
            )
            .is_err()
        );
    }

    #[test]
    fn normalized_configs_remain_fully_explicit_when_resubmitted() {
        let input = AgentConfigInput::new(
            ConfigId {
                config_group: ConfigGroup::Global,
                name: "agent".into(),
            },
            vec![ModelSelection::Global],
        );
        let explicit: AgentConfigInput = AgentDefaults::default().resolve(input).into();
        let serialized = serde_json::to_value(&explicit).unwrap();
        let roundtrip: AgentConfigInput = serde_json::from_value(serialized).unwrap();
        let resolved = defaults().resolve(roundtrip);
        assert!(resolved.bundles.is_empty());
    }
}
