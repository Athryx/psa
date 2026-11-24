//! Format of agent definition files (`agents/*.toml` in a config group).
//! See `docs/AGENT_DEF_SCHEMA.md`.

#[cfg(test)]
mod tests;

use crate::server::AgentDefaults;
use psa::api::{
    AgentConfig, AgentConfigInput, BundleConfig, ConfigGroup, ConfigId, IncludedItems,
    ModelSelection,
};
use serde::Deserialize;
use std::{collections::HashSet, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum AgentDefinitionError {
    #[error(transparent)]
    Parse(#[from] toml::de::Error),
    #[error("name and models must not be empty")]
    Empty,
}

/// TOML input defaults differ deliberately from the API's required model list.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinitionConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub selectable: bool,
    #[serde(default)]
    pub bundles: Option<Vec<BundleConfig>>,
    #[serde(default = "default_models")]
    pub models: Vec<ModelSelection>,
    #[serde(default = "yes", alias = "default")]
    pub first_model_default: bool,
    #[serde(default)]
    pub subagents: SubagentInclusionsConfig,
    #[serde(default)]
    pub skills: SkillInclusionsConfig,
}
fn yes() -> bool {
    true
}
fn default_models() -> Vec<ModelSelection> {
    vec![ModelSelection::Global]
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentInclusionsConfig {
    pub global_defaults: bool,
    /// Include every item of this config's own group.
    pub group_defaults: bool,
    pub subagents: Vec<String>,
}
impl Default for SubagentInclusionsConfig {
    fn default() -> Self {
        Self {
            global_defaults: true,
            group_defaults: true,
            subagents: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillInclusionsConfig {
    pub global_defaults: bool,
    /// Include every item of this config's own group.
    pub group_defaults: bool,
    pub skills: Vec<String>,
    pub additional_paths: Vec<PathBuf>,
}
impl Default for SkillInclusionsConfig {
    fn default() -> Self {
        Self {
            global_defaults: true,
            group_defaults: true,
            skills: Vec::new(),
            additional_paths: Vec::new(),
        }
    }
}

impl AgentDefinitionConfig {
    /// Parse an agent definition file, requiring a name and at least one model.
    pub fn parse(text: &str) -> Result<Self, AgentDefinitionError> {
        let config: Self = toml::from_str(text)?;
        if config.name.trim().is_empty() || config.models.is_empty() {
            return Err(AgentDefinitionError::Empty);
        }
        Ok(config)
    }

    /// Build the API configuration for this definition in `group`. Named
    /// inclusions resolve against the discovered `agents` and `skills`;
    /// `additional_skills` are the skills found at `skills.additional_paths`.
    /// Omitted bundles come from `defaults`. Bundles are not validated here.
    pub fn into_agent_config(
        self,
        group: &ConfigGroup,
        additional_skills: Vec<ConfigId>,
        agents: &HashSet<ConfigId>,
        skills: &HashSet<ConfigId>,
        defaults: &AgentDefaults,
    ) -> AgentConfig {
        let subagents = expand_inclusions(
            self.subagents.global_defaults,
            self.subagents.group_defaults,
            &self.subagents.subagents,
            group,
            agents,
        );
        let mut included_skills = expand_inclusions(
            self.skills.global_defaults,
            self.skills.group_defaults,
            &self.skills.skills,
            group,
            skills,
        );
        included_skills.items.extend(additional_skills);
        defaults.resolve(AgentConfigInput {
            config_id: ConfigId {
                config_group: group.clone(),
                name: self.name,
            },
            description: self.description,
            selectable: self.selectable,
            bundles: self.bundles,
            models: self.models,
            first_model_default: self.first_model_default,
            subagents,
            skills: included_skills,
        })
    }
}

/// Default groups are Global and/or the config's own group; named items
/// resolve in the own group first, then in Global. A Global config's own-group
/// defaults also select the creating session's `.agents` group.
pub fn expand_inclusions(
    global: bool,
    own_group: bool,
    names: &[String],
    current: &ConfigGroup,
    available: &HashSet<ConfigId>,
) -> IncludedItems {
    let mut groups = Vec::new();
    if global {
        groups.push(ConfigGroup::Global);
    }
    if own_group && !groups.contains(current) {
        groups.push(current.clone());
    }
    if own_group && *current == ConfigGroup::Global {
        groups.push(ConfigGroup::Session);
    }
    let items = names
        .iter()
        .map(|name| {
            let local = ConfigId {
                config_group: current.clone(),
                name: name.clone(),
            };
            if available.contains(&local) {
                local
            } else {
                ConfigId {
                    config_group: ConfigGroup::Global,
                    name: name.clone(),
                }
            }
        })
        .collect();
    IncludedItems { groups, items }
}
