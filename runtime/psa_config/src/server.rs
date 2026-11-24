//! Schema and defaults of the server's `config.toml`.

use crate::PathIoError;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa::api::{AgentConfig, AgentConfigInput, BundleConfig};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    net::SocketAddr,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

/// Decision calls in flight at once per session unless `[decisions]` sets
/// `max_concurrent_calls`; further calls wait for a slot.
pub const DEFAULT_MAX_CONCURRENT_DECISIONS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

pub const DEFAULT_CONFIG: &str = r#"host = true
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
pub const DEFAULT_AGENTS: [(&str, &str); 2] = [
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Marks the machine running the server, so its probe gets host privileges.
    #[serde(default)]
    pub host: bool,
    #[serde(default = "default_agent_name")]
    pub default_agent_name: String,
    pub server: ServerSection,
    pub database: DatabaseSection,
    #[serde(
        default = "default_model_config",
        deserialize_with = "deserialize_session_defaults"
    )]
    pub session_defaults: ModelConfig,
    #[serde(default)]
    pub agent_defaults: AgentDefaults,
    #[serde(default)]
    pub sandboxes: SandboxSection,
    #[serde(default)]
    pub decisions: DecisionsSection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub listen: SocketAddr,
    #[serde(default)]
    pub autostart_local_probe: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSection {
    /// Relative paths resolve against the data directory.
    pub path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSection {
    /// Docker image for sandboxes created without an explicit image.
    pub default_image: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionsSection {
    #[serde(default = "default_max_concurrent_decision_calls")]
    pub max_concurrent_calls: NonZeroUsize,
}

impl Default for DecisionsSection {
    fn default() -> Self {
        Self {
            max_concurrent_calls: default_max_concurrent_decision_calls(),
        }
    }
}

impl ServerConfig {
    /// Parse a configuration. Bundle lists are not validated here.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Read only the `host` marker, ignoring every other server field. A missing
/// file means the machine is not the host.
pub async fn read_host_marker(path: &Path) -> Result<bool, HostMarkerError> {
    // Ignore unrelated server fields: only the root host marker matters here.
    #[derive(Deserialize)]
    struct HostMarker {
        #[serde(default)]
        host: bool,
    }

    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let marker: HostMarker = toml::from_str(&text).map_err(|_| HostMarkerError::Invalid)?;
    Ok(marker.host)
}

#[derive(Debug, thiserror::Error)]
pub enum HostMarkerError {
    #[error("cannot read server config: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config.toml")]
    Invalid,
}

fn default_agent_name() -> String {
    "root".into()
}

fn default_max_concurrent_decision_calls() -> NonZeroUsize {
    DEFAULT_MAX_CONCURRENT_DECISIONS
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
pub async fn ensure_default_config(path: &Path, data_dir: &Path) -> Result<(), PathIoError> {
    if !write_new_file(path, DEFAULT_CONFIG).await? {
        return Ok(());
    }

    let agents = data_dir.join(crate::paths::AGENTS_DIR);
    tokio::fs::create_dir_all(&agents)
        .await
        .map_err(|source| PathIoError {
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
async fn write_new_file(path: &Path, contents: &str) -> Result<bool, PathIoError> {
    let io_error = |source| PathIoError {
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
