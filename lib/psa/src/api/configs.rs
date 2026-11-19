//! Named, mutable agent configurations and their registration input.

use crate::api::{BundleConfig, ProbeId, SessionId};
use llm_api::providers::{Effort, Model};
use serde::{Deserialize, Serialize};

/// Server options safe to expose to authenticated clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Name of the agent configuration suggested for new sessions.
    pub default_agent_name: String,
    /// Docker image used for new sandboxes that do not name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sandbox_image: Option<String>,
}

/// Namespace for named agent configurations and skills.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ConfigGroup {
    /// Caller-managed namespace, independent of filesystem discovery.
    Custom(String),
    /// Server-wide filesystem-discovered namespace.
    Global,
    /// Probe-local namespace discovered from an `.agents` directory, found by
    /// walking up from a session's working directory. Unrelated to
    /// [`crate::api::ProjectId`], which only labels sessions.
    AgentsDir {
        /// Absolute `.agents` directory path.
        path: String,
        /// Probe that owns the directory.
        probe_id: ProbeId,
    },
    /// Placeholder for the creating session's discovered `.agents` group, valid
    /// only in `IncludedItems::groups`. It selects nothing when the session has
    /// no `.agents` directory or outside agent creation.
    Session,
}

/// Mutable configuration identity, distinct from an immutable snapshot ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigId {
    /// Namespace containing the configuration.
    pub config_group: ConfigGroup,
    /// Name unique within the namespace.
    pub name: String,
}

/// Name-keyed inclusion selection; later groups override earlier groups,
/// then explicit items override groups. Omitted lists select nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IncludedItems {
    /// Ordered namespaces whose registered items are included.
    pub groups: Vec<ConfigGroup>,
    /// Explicit identities overriding group selections with the same name.
    pub items: Vec<ConfigId>,
}

/// Allowed model policy. `Global` parts keep following the session model after
/// creation; `Parent` is fixed to the parent's policy when the agent is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum ModelSelection {
    /// Use an explicit provider/model with a separate effort policy.
    Fixed {
        /// Pinned provider model, written as its slug, such as `openrouter/vendor/model`.
        model: Model,
        /// Reasoning effort policy for this model.
        effort: EffortSelection,
    },
    /// Follow the session's model configuration.
    Global,
    /// Use the parent's model policy; root agents follow the session.
    Parent,
}

/// Reasoning effort policy for a fixed model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EffortSelection {
    /// Use the specified effort.
    Fixed(Effort),
    /// Use the parent's effort policy; root agents follow the session.
    Parent,
    /// Follow the session's effort.
    Global,
}

/// Unresolved current input, upserted by `config_id`. Models must be nonempty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Identity to create or replace on upsert.
    pub config_id: ConfigId,
    /// Human-readable description; defaults to empty.
    #[serde(default)]
    pub description: String,
    /// Selection metadata; defaults to false and does not install bundles.
    #[serde(default)]
    pub selectable: bool,
    /// Ordered bundle construction configurations; omitted lists are empty, not inherited.
    #[serde(default)]
    pub bundles: Vec<BundleConfig>,
    /// Required nonempty ordered list of allowed model policies.
    pub models: Vec<ModelSelection>,
    /// Use the first model when no choice is supplied; defaults to true.
    /// Registration normalizes this to true when there is only one choice.
    /// Accepts the former name `default`.
    #[serde(default = "default_model_choice", alias = "default")]
    pub first_model_default: bool,
    /// Permitted subagent configuration selection; defaults to empty.
    #[serde(default)]
    pub subagents: IncludedItems,
    /// Skill selection for snapshot resolution; defaults to empty.
    #[serde(default)]
    pub skills: IncludedItems,
}

fn default_model_choice() -> bool {
    true
}

/// API upsert input. Component omissions are resolved by the server, unlike the
/// fully specified `AgentConfig` returned by lookup and registration operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfigInput {
    /// Named configuration identity to create or replace.
    pub config_id: ConfigId,
    /// Human-readable description; defaults to empty.
    #[serde(default)]
    pub description: String,
    /// Selection metadata; defaults to false.
    #[serde(default)]
    pub selectable: bool,
    /// Omission/null inherits server bundles; explicit lists replace them, even empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundles: Option<Vec<BundleConfig>>,
    /// Required nonempty ordered list of allowed model policies.
    pub models: Vec<ModelSelection>,
    /// Use the first model when no choice is supplied; defaults to true.
    /// Registration normalizes this to true when there is only one choice.
    /// Accepts the former name `default`.
    #[serde(default = "enabled", alias = "default")]
    pub first_model_default: bool,
    /// Permitted subagent selection; defaults to empty.
    #[serde(default)]
    pub subagents: IncludedItems,
    /// Skill inclusion selection; defaults to empty.
    #[serde(default)]
    pub skills: IncludedItems,
}

fn enabled() -> bool {
    true
}

impl AgentConfigInput {
    /// Build input that inherits the default bundles and permits the first model
    /// as the default choice. Other metadata and inclusion selections start empty.
    /// The caller must supply a nonempty model list; this constructor does not validate it.
    pub fn new(config_id: ConfigId, models: Vec<ModelSelection>) -> Self {
        Self {
            config_id,
            models,
            description: String::new(),
            selectable: false,
            bundles: None,
            first_model_default: true,
            subagents: IncludedItems::default(),
            skills: IncludedItems::default(),
        }
    }
}

/// A normalized config is fully explicit, including an empty bundle list.
impl From<AgentConfig> for AgentConfigInput {
    fn from(config: AgentConfig) -> Self {
        Self {
            config_id: config.config_id,
            description: config.description,
            selectable: config.selectable,
            bundles: Some(config.bundles),
            models: config.models,
            first_model_default: config.first_model_default,
            subagents: config.subagents,
            skills: config.skills,
        }
    }
}

/// Unresolved explicit dependency of a configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissingComponent {
    /// Referenced agent configuration is unavailable.
    Agent {
        /// Missing configuration identity.
        config_id: ConfigId,
    },
    /// Referenced skill is unavailable.
    Skill {
        /// Missing skill configuration identity.
        config_id: ConfigId,
    },
}

/// Current dependency-resolution status, separate from snapshot identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionStatus {
    /// Missing dependencies; empty when all selected dependencies are available.
    pub missing_components: Vec<MissingComponent>,
}

/// Current named configuration paired with its dependency status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigStatus {
    /// Current registered configuration.
    pub config: AgentConfig,
    /// Current dependency availability.
    pub status: AgentDefinitionStatus,
}

/// Rescan filesystem-discovered configuration. The server's global directory is
/// always rescanned; each listed session also has its `.agents` directory found
/// (on first use) and rescanned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshConfigs {
    /// Sessions whose `.agents` directories are also rescanned; may be empty.
    pub session_ids: Vec<SessionId>,
}

/// A [`ConfigId`] flattened into query parameters for `GET` lookups. `custom`
/// names a `Custom` group; `path` and `probe_id` locate an `AgentsDir` group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigLookup {
    /// Configuration name within its group.
    pub name: String,
    /// Which kind of group contains the configuration.
    pub group: ConfigGroupKind,
    /// `Custom` group name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
    /// `AgentsDir` directory path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `AgentsDir` owning probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_id: Option<ProbeId>,
}

/// The variant of a [`ConfigGroup`], without its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigGroupKind {
    /// [`ConfigGroup::Custom`].
    Custom,
    /// [`ConfigGroup::Global`].
    Global,
    /// [`ConfigGroup::AgentsDir`].
    AgentsDir,
    /// [`ConfigGroup::Session`].
    Session,
}

impl From<&ConfigId> for ConfigLookup {
    fn from(id: &ConfigId) -> Self {
        let mut lookup = Self {
            name: id.name.clone(),
            group: ConfigGroupKind::Global,
            custom: None,
            path: None,
            probe_id: None,
        };
        match &id.config_group {
            ConfigGroup::Custom(name) => {
                lookup.group = ConfigGroupKind::Custom;
                lookup.custom = Some(name.clone());
            }
            ConfigGroup::Global => {}
            ConfigGroup::AgentsDir { path, probe_id } => {
                lookup.group = ConfigGroupKind::AgentsDir;
                lookup.path = Some(path.clone());
                lookup.probe_id = Some(*probe_id);
            }
            ConfigGroup::Session => lookup.group = ConfigGroupKind::Session,
        }
        lookup
    }
}

impl TryFrom<ConfigLookup> for ConfigId {
    type Error = String;

    /// Requires exactly the parameters the group kind uses.
    fn try_from(lookup: ConfigLookup) -> Result<Self, Self::Error> {
        let config_group = match (lookup.group, lookup.custom, lookup.path, lookup.probe_id) {
            (ConfigGroupKind::Custom, Some(name), None, None) => ConfigGroup::Custom(name),
            (ConfigGroupKind::Global, None, None, None) => ConfigGroup::Global,
            (ConfigGroupKind::AgentsDir, None, Some(path), Some(probe_id)) => {
                ConfigGroup::AgentsDir { path, probe_id }
            }
            (ConfigGroupKind::Session, None, None, None) => ConfigGroup::Session,
            (group, ..) => {
                return Err(format!(
                    "{group:?} lookups take `custom` only for Custom groups and \
                     `path` with `probe_id` only for AgentsDir groups"
                ));
            }
        };
        Ok(Self {
            config_group,
            name: lookup.name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CreateAgent, NewSkill};
    use serde_json::json;

    #[test]
    fn config_lookups_round_trip_every_group_and_reject_stray_parameters() {
        let probe_id = ProbeId(uuid::Uuid::nil());
        for config_group in [
            ConfigGroup::Custom("team".into()),
            ConfigGroup::Global,
            ConfigGroup::AgentsDir {
                path: "/work/.agents".into(),
                probe_id,
            },
            ConfigGroup::Session,
        ] {
            let id = ConfigId {
                config_group,
                name: "agent".into(),
            };
            assert_eq!(ConfigId::try_from(ConfigLookup::from(&id)), Ok(id));
        }

        let mut lookup = ConfigLookup::from(&ConfigId {
            config_group: ConfigGroup::Global,
            name: "agent".into(),
        });
        lookup.path = Some("/work/.agents".into());
        assert!(ConfigId::try_from(lookup.clone()).is_err());
        lookup.group = ConfigGroupKind::AgentsDir;
        assert!(ConfigId::try_from(lookup.clone()).is_err());
        lookup.probe_id = Some(probe_id);
        assert!(ConfigId::try_from(lookup).is_ok());
    }

    #[test]
    fn configuration_requests_have_explicit_models_and_closed_nested_inputs() {
        let id = json!({"config_group": {"Custom": "test"}, "name": "agent"});
        let config: AgentConfig =
            serde_json::from_value(json!({"config_id": id, "models": ["Global"]})).unwrap();
        assert!(config.first_model_default);
        let legacy: AgentConfig = serde_json::from_value(
            json!({"config_id": id, "models": ["Global"], "default": false}),
        )
        .unwrap();
        assert!(!legacy.first_model_default);
        assert!(config.subagents.groups.is_empty());
        assert!(serde_json::from_value::<AgentConfig>(json!({"config_id": id})).is_err());
        assert!(
            serde_json::from_value::<ConfigId>(
                json!({"config_group": "Global", "name": "x", "typo": true})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<ModelSelection>(json!({"Fixed": {"model": "openrouter/example/model", "effort": "Global", "typo": true}})).is_err());
        assert!(
            serde_json::from_value::<NewSkill>(
                json!({"config_id": id, "tar_data": "", "path": "/server/path"})
            )
            .is_err()
        );
        let create: CreateAgent =
            serde_json::from_value(json!({"config_id": id, "name": "instance"})).unwrap();
        assert!(create.model.is_none());
        assert!(!create.interactive);
        assert!(!create.hidden);
    }
}
