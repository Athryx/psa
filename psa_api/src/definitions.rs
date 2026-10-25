//! Mutable named configuration and metadata for immutable definition snapshots.
//! Executable snapshot fields are defined by `AgentDefinition`.
use crate::{
    AgentDefinition, AgentDefinitionId, AgentId, BundleConfig, HookConfigs, ProbeId, SessionId,
};
use llm_api::providers::Effort;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Namespace for named agent configurations and skills.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ConfigGroup {
    /// Caller-managed namespace, independent of filesystem discovery.
    Custom(#[doc = "Namespace name."] String),
    /// Server-wide filesystem-discovered namespace.
    Global,
    /// Probe-local project namespace discovered from an `.agents` directory.
    Project {
        /// Absolute project configuration directory path.
        path: String,
        /// Probe that owns the project directory.
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

/// Allowed model policy, resolved once when an agent is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum ModelSelection {
    /// Use an explicit provider/model with a separate effort policy.
    Fixed {
        /// Provider slug followed by the API name, such as `openrouter/vendor/model`.
        model: String,
        /// Reasoning effort policy for this model.
        effort: EffortSelection,
    },
    /// Copy the session's current model configuration.
    Global,
    /// Copy the parent's model configuration, falling back to the session's.
    Parent,
}

/// Reasoning effort policy resolved at agent creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EffortSelection {
    /// Use the specified effort.
    Fixed(#[doc = "Configured reasoning effort."] Effort),
    /// Copy the parent's effort, falling back to the session's.
    Parent,
    /// Copy the session's current effort.
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

/// Stable mutable skill record identity, not a content revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(#[doc = "Numeric skill record identifier."] pub u64);

/// Trusted filesystem source of a skill's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLocation {
    /// Server-owned skill source, materialized on probes before use.
    Global(#[doc = "Server-local skill path."] PathBuf),
    /// Skill source already owned by a probe.
    Probe {
        /// Probe holding the source.
        id: ProbeId,
        /// Probe-local skill path.
        path: PathBuf,
    },
}

/// Registered mutable skill metadata and its trusted source location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Stable record identity retained across updates.
    pub id: SkillId,
    /// Named configuration identity used for inclusion.
    pub config_id: ConfigId,
    /// Skill name parsed from its metadata.
    pub name: String,
    /// Skill description parsed from its metadata.
    pub description: String,
    /// Filesystem source used for materialization.
    pub skill_location: SkillLocation,
    /// Exclude from the model-facing skill catalogue; defaults to false.
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Trusted source registration after reading and validating SKILL.md.
/// Network upload callers use `NewSkill`, not arbitrary server-local paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistration {
    /// Named identity to create or update.
    pub config_id: ConfigId,
    /// Validated skill name.
    pub name: String,
    /// Validated skill description.
    pub description: String,
    /// Trusted server or probe source location.
    pub skill_location: SkillLocation,
    /// Exclude from the model-facing catalogue; defaults to false.
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// Upload a skill archive rather than asserting a server-local path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSkill {
    /// Destination identity; its name must match the skill metadata name.
    pub config_id: ConfigId,
    /// Uncompressed tar archive containing a root `SKILL.md` and support files.
    pub tar_data: Vec<u8>,
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

/// Resolved references kept separate from executable snapshot fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedDefinitionReferences {
    /// Immutable permitted subagent snapshots.
    pub subagents: Vec<AgentDefinitionId>,
    /// Mutable skill record identities, not executable skill paths.
    pub skills: Vec<SkillId>,
}

/// Snapshot source policies plus executable fields. Status is current,
/// not part of snapshot identity; skills reference mutable records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAgentConfig {
    /// Source configuration policies captured by this snapshot.
    pub config: AgentConfig,
    /// Immutable executable fields, including materialized skill paths.
    pub definition: AgentDefinition,
    /// Resolved subagent snapshots and mutable skill record IDs.
    pub references: ResolvedDefinitionReferences,
}

/// Create an agent by resolving and pinning a current named configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewConfiguredAgent {
    /// Configuration whose current reachable graph is resolved.
    pub config_id: ConfigId,
    /// Display name for this agent instance.
    pub name: String,
    /// Optional subagent parent; omission means no parent.
    #[serde(default)]
    pub subagent_parent_id: Option<AgentId>,
    /// Metadata available to hooks and tools; defaults to false.
    #[serde(default)]
    pub interactive: bool,
    /// Client visibility metadata only; defaults to false without server filtering.
    #[serde(default)]
    pub hidden: bool,
    /// Selection that must exactly match an allowed entry, including effort policy.
    /// Omission/null selects the sole entry, or the first entry when defaults are enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
}

/// Discovery refresh followed by a listing of all registered skills.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshAndListSkills {
    /// Refresh this session's project in addition to globals before listing the entire
    /// registry. `None` refreshes only globals; registered project records remain listed.
    pub refresh_session_id: Option<SessionId>,
}

/// Discovery refresh followed by a listing of all registered configurations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshAndListAgentConfigs {
    /// Refresh this session's project in addition to globals before listing the entire
    /// registry. `None` refreshes only globals; registered project records remain listed.
    pub refresh_session_id: Option<SessionId>,
}

/// Consistent reachable selection for adapters to materialize outside a DB lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigGraph {
    /// Reachable source configurations.
    pub configs: Vec<AgentConfig>,
    /// Selected mutable skill records to materialize.
    pub skills: Vec<Skill>,
    /// Dependency statuses captured with the graph.
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
        assert!(!create.hidden);
    }
}
