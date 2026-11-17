//! Named, mutable agent configurations and their registration input.

use crate::{BundleConfig, HookConfig, HookConfigs, ProbeId, SessionId};
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
    /// [`crate::ProjectId`], which only labels sessions.
    AgentsDir {
        /// Absolute `.agents` directory path.
        path: String,
        /// Probe that owns the directory.
        probe_id: ProbeId,
    },
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
    /// Selection metadata; defaults to false and does not install tools or hooks.
    #[serde(default)]
    pub selectable: bool,
    /// Ordered bundle construction configurations; omitted lists are empty, not inherited.
    #[serde(default)]
    pub bundles: Vec<BundleConfig>,
    /// Explicit lifecycle hook configurations; omitted stages are empty.
    #[serde(default)]
    pub hooks: HookConfigs,
    /// Required nonempty ordered list of allowed model policies.
    pub models: Vec<ModelSelection>,
    /// Allow the first model when no choice is supplied; defaults to true.
    /// Registration normalizes this to true when there is only one choice.
    #[serde(default = "default_model_choice")]
    pub default: bool,
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
    /// Per-stage overrides; absent stages independently inherit server defaults.
    #[serde(default)]
    pub hooks: HookOverrides,
    /// Required nonempty ordered list of allowed model policies.
    pub models: Vec<ModelSelection>,
    /// Select the first model when no choice is supplied; defaults to true.
    /// Registration normalizes this to true when there is only one choice.
    #[serde(default = "enabled")]
    pub default: bool,
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
    /// Build input that inherits all component defaults and permits the first model
    /// as the default choice. Other metadata and inclusion selections start empty.
    /// The caller must supply a nonempty model list; this constructor does not validate it.
    pub fn new(config_id: ConfigId, models: Vec<ModelSelection>) -> Self {
        Self {
            config_id,
            models,
            description: String::new(),
            selectable: false,
            bundles: None,
            hooks: HookOverrides::default(),
            default: true,
            subagents: IncludedItems::default(),
            skills: IncludedItems::default(),
        }
    }
}

/// A normalized config is fully explicit, including empty component lists.
impl From<AgentConfig> for AgentConfigInput {
    fn from(config: AgentConfig) -> Self {
        Self {
            config_id: config.config_id,
            description: config.description,
            selectable: config.selectable,
            bundles: Some(config.bundles),
            hooks: config.hooks.into(),
            models: config.models,
            default: config.default,
            subagents: config.subagents,
            skills: config.skills,
        }
    }
}

/// Each absent stage inherits independently; Some([]) disables that stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookOverrides {
    /// Creation hooks; omission inherits defaults and an empty list disables them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_create: Option<Vec<HookConfig>>,
    /// Session-update hooks; omission inherits defaults and an empty list disables them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_update: Option<Vec<HookConfig>>,
    /// Before-request hooks; omission inherits defaults and an empty list disables them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_request: Option<Vec<HookConfig>>,
    /// After-request hooks; omission inherits defaults and an empty list disables them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_request: Option<Vec<HookConfig>>,
}

impl From<HookConfigs> for HookOverrides {
    fn from(hooks: HookConfigs) -> Self {
        Self {
            agent_create: Some(hooks.agent_create),
            session_update: Some(hooks.session_update),
            before_request: Some(hooks.before_request),
            after_request: Some(hooks.after_request),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateAgent, NewSkill};
    use serde_json::json;

    #[test]
    fn configuration_requests_have_explicit_models_and_closed_nested_inputs() {
        let id = json!({"config_group": {"Custom": "test"}, "name": "agent"});
        let config: AgentConfig =
            serde_json::from_value(json!({"config_id": id, "models": ["Global"]})).unwrap();
        assert!(config.default);
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
                json!({"config_id": id, "tar_data": [], "path": "/server/path"})
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
