//! Immutable agent definition snapshots and their model policy.

use crate::api::{AgentConfig, BundleConfig, HookConfigs, SkillId};
use llm_api::providers::{Effort, Model, ModelConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Identifier of an immutable agent definition version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentDefinitionId(pub u64);

/// Model policy of an agent definition version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentModel {
    /// Follow the session's current model and effort.
    #[default]
    Global,
    /// Use a pinned model with its own effort policy.
    Fixed {
        /// Pinned provider model.
        model: Model,
        /// Reasoning effort policy for the pinned model.
        effort: AgentEffort,
    },
}

/// Reasoning effort policy of a pinned agent model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEffort {
    /// Follow the session's current effort.
    Global,
    /// Use the specified effort.
    Fixed(Effort),
}

impl AgentModel {
    /// The concrete configuration this policy selects under the session model.
    pub fn resolve(&self, session: &ModelConfig) -> ModelConfig {
        match self {
            Self::Global => session.clone(),
            Self::Fixed { model, effort } => ModelConfig {
                model: model.clone(),
                effort: match effort {
                    AgentEffort::Global => session.effort,
                    AgentEffort::Fixed(effort) => *effort,
                },
            },
        }
    }

    /// The effort policy this model policy applies.
    pub fn effort(&self) -> AgentEffort {
        match self {
            Self::Global => AgentEffort::Global,
            Self::Fixed { effort, .. } => *effort,
        }
    }
}

impl From<ModelConfig> for AgentModel {
    fn from(config: ModelConfig) -> Self {
        Self::Fixed {
            model: config.model,
            effort: AgentEffort::Fixed(config.effort),
        }
    }
}

/// An immutable, versioned agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Identifier of this definition version.
    pub id: AgentDefinitionId,
    /// Previous immutable version, if this definition replaces one.
    pub previous_version_id: Option<AgentDefinitionId>,
    /// Version number within the definition's version chain.
    pub version: u64,
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this definition is offered for selection.
    pub selectable: bool,
    /// Ordered feature bundle construction configurations.
    pub bundles: Vec<BundleConfig>,
    /// Ordered hook construction configurations for each lifecycle stage.
    pub hooks: HookConfigs,
    /// Model policy, re-resolved against the session model whenever it changes.
    pub model: AgentModel,
    /// Paths to skill files or directories.
    pub skills: Vec<PathBuf>,
    /// Definition versions this agent may use for subagents.
    pub permitted_subagents: Vec<AgentDefinitionId>,
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
