//! Registered skills and skill uploads.

use crate::{ConfigId, ProbeId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable mutable skill record identity, not a content revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub u64);

/// Trusted filesystem source of a skill's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLocation {
    /// Server-owned skill source, materialized on probes before use.
    Global(PathBuf),
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

/// Upload a skill archive rather than asserting a server-local path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSkill {
    /// Destination identity; its name must match the skill metadata name.
    pub config_id: ConfigId,
    /// Uncompressed tar archive containing a root `SKILL.md` and support files.
    pub tar_data: Vec<u8>,
}
