//! Mutable named configuration and metadata for immutable definition snapshots.
//! Executable snapshot fields are defined by `AgentDefinition`.
use crate::{
    AgentDefinition, AgentDefinitionId, AgentId, BundleId, HookIds, ProbeId, SessionId, ToolId,
};
use llm_api::providers::Effort;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ConfigGroup {
    Custom(String),
    Global,
    Project { path: String, probe_id: ProbeId },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigId {
    pub config_group: ConfigGroup,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IncludedItems {
    pub groups: Vec<ConfigGroup>,
    pub items: Vec<ConfigId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ModelSelection {
    Fixed {
        model: String,
        effort: EffortSelection,
    },
    Global,
    Parent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffortSelection {
    Fixed(Effort),
    Parent,
    Global,
}

/// Unresolved current input, upserted by `config_id`. Models must be nonempty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub config_id: ConfigId,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub selectable: bool,
    #[serde(default)]
    pub bundles: Vec<BundleId>,
    #[serde(default)]
    pub tools: Vec<ToolId>,
    #[serde(default)]
    pub hooks: HookIds,
    #[serde(default)]
    pub extra_system_prompt: String,
    pub models: Vec<ModelSelection>,
    #[serde(default = "default_model_choice")]
    pub default: bool,
    #[serde(default)]
    pub subagents: IncludedItems,
    #[serde(default)]
    pub skills: IncludedItems,
}

fn default_model_choice() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLocation {
    Global(PathBuf),
    Probe { id: ProbeId, path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub config_id: ConfigId,
    pub name: String,
    pub description: String,
    pub skill_location: SkillLocation,
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Trusted source registration after reading and validating SKILL.md.
/// Network upload callers use `NewSkill`, not arbitrary server-local paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistration {
    pub config_id: ConfigId,
    pub name: String,
    pub description: String,
    pub skill_location: SkillLocation,
    #[serde(default)]
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSkill {
    pub config_id: ConfigId,
    pub tar_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissingComponent {
    Agent { config_id: ConfigId },
    Skill { config_id: ConfigId },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionStatus {
    pub missing_components: Vec<MissingComponent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigStatus {
    pub config: AgentConfig,
    pub status: AgentDefinitionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedDefinitionReferences {
    pub subagents: Vec<AgentDefinitionId>,
    pub skills: Vec<SkillId>,
}

/// Snapshot source policies plus executable fields. Status is current,
/// not part of snapshot identity; skills reference mutable records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAgentConfig {
    pub config: AgentConfig,
    pub definition: AgentDefinition,
    pub references: ResolvedDefinitionReferences,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewConfiguredAgent {
    pub config_id: ConfigId,
    pub name: String,
    #[serde(default)]
    pub subagent_parent_id: Option<AgentId>,
    #[serde(default)]
    pub interactive: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSkills {
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListAgentConfigs {
    pub session_id: Option<SessionId>,
}

/// Consistent reachable selection for adapters to materialize outside a DB lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigGraph {
    pub configs: Vec<AgentConfig>,
    pub skills: Vec<Skill>,
    pub statuses: Vec<AgentConfigStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let create: NewConfiguredAgent =
            serde_json::from_value(json!({"config_id": id, "name": "instance"})).unwrap();
        assert!(create.model.is_none());
        assert!(!create.interactive);
    }
}
