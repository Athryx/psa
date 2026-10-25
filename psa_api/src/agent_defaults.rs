//! Omission-aware configuration input and shared component default resolution.
use crate::{
    AgentConfig, BundleConfig, ConfigId, HookConfig, HookConfigs, IncludedItems, ModelSelection,
};
use serde::{Deserialize, Serialize};

/// Server-configured fallbacks. Explicit definition lists replace these lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentDefaults {
    /// Ordered fallback bundle configurations; defaults to empty.
    pub bundles: Vec<BundleConfig>,
    /// Per-stage fallback hook configurations; all stages default to empty.
    pub hooks: HookConfigs,
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

impl HookOverrides {
    /// Replace absent stages with defaults, preserving explicit lists unchanged.
    pub fn resolve(self, defaults: &HookConfigs) -> HookConfigs {
        HookConfigs {
            agent_create: self
                .agent_create
                .unwrap_or_else(|| defaults.agent_create.clone()),
            session_update: self
                .session_update
                .unwrap_or_else(|| defaults.session_update.clone()),
            before_request: self
                .before_request
                .unwrap_or_else(|| defaults.before_request.clone()),
            after_request: self
                .after_request
                .unwrap_or_else(|| defaults.after_request.clone()),
        }
    }
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

    /// Normalize omissions using server defaults without merging explicit lists.
    /// This does not validate models or resolve dependency and model policies.
    pub fn resolve(self, defaults: &AgentDefaults) -> AgentConfig {
        AgentConfig {
            config_id: self.config_id,
            description: self.description,
            selectable: self.selectable,
            bundles: self.bundles.unwrap_or_else(|| defaults.bundles.clone()),
            hooks: self.hooks.resolve(&defaults.hooks),
            models: self.models,
            default: self.default,
            subagents: self.subagents,
            skills: self.skills,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookConfig;
    use serde_json::json;

    fn defaults() -> AgentDefaults {
        AgentDefaults {
            bundles: vec![
                BundleConfig::FileTools { read_only: false },
                BundleConfig::Noop,
            ],
            hooks: HookConfigs {
                agent_create: vec![
                    HookConfig::DefaultStartup,
                    HookConfig::ExtraSystemPrompt {
                        prompt: "Inherited prompt".into(),
                    },
                ],
                after_request: vec![HookConfig::StopWithoutTools],
                ..Default::default()
            },
        }
    }

    #[test]
    fn omitted_lists_inherit_and_explicit_empty_lists_disable_defaults() {
        let id = json!({"config_group": "Global", "name": "agent"});
        let input: AgentConfigInput =
            serde_json::from_value(json!({"config_id": id, "models": ["Global"]})).unwrap();
        let resolved = input.clone().resolve(&defaults());
        assert_eq!(resolved.bundles, defaults().bundles);
        assert_eq!(resolved.hooks, defaults().hooks);
        let wire = serde_json::to_value(&input).unwrap();
        assert!(wire.get("bundles").is_none());
        assert!(wire.get("tools").is_none());
        let input: AgentConfigInput = serde_json::from_value(json!({"config_id": id, "models": ["Global"], "bundles": [], "hooks": {"agent_create": []}})).unwrap();
        let resolved = input.resolve(&defaults());
        assert!(resolved.bundles.is_empty());
        assert!(resolved.hooks.agent_create.is_empty());
        assert_eq!(resolved.hooks.after_request, defaults().hooks.after_request);
    }

    #[test]
    fn explicit_hook_payloads_replace_only_their_stage() {
        let input: AgentConfigInput = serde_json::from_value(json!({
            "config_id": {"config_group": "Global", "name": "agent"},
            "models": ["Global"],
            "hooks": {"agent_create": [{"ExtraSystemPrompt": {"prompt": "Local prompt"}}]},
        }))
        .unwrap();
        let serialized = serde_json::to_value(&input).unwrap();
        assert!(serialized["hooks"].get("after_request").is_none());
        let resolved = input.resolve(&defaults());
        assert_eq!(
            resolved.hooks.agent_create,
            vec![HookConfig::ExtraSystemPrompt {
                prompt: "Local prompt".into(),
            }]
        );
        assert_eq!(resolved.hooks.after_request, defaults().hooks.after_request);
        assert_eq!(resolved.bundles, defaults().bundles);
    }

    #[test]
    fn normalized_configs_remain_fully_explicit_when_resubmitted() {
        let input = AgentConfigInput::new(
            ConfigId {
                config_group: crate::ConfigGroup::Global,
                name: "agent".into(),
            },
            vec![ModelSelection::Global],
        );
        let explicit: AgentConfigInput = input.resolve(&AgentDefaults::default()).into();
        let serialized = serde_json::to_value(&explicit).unwrap();
        let roundtrip: AgentConfigInput = serde_json::from_value(serialized).unwrap();
        let resolved = roundtrip.resolve(&defaults());
        assert!(resolved.bundles.is_empty());
        assert_eq!(resolved.hooks, HookConfigs::default());
    }
}
