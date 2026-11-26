//! The seam through which the runtime reads agent configuration that lives in
//! the server's data directory and on probes.

use crate::probes::ProbeHandle;
use futures_util::future::BoxFuture;
use psa::api::{ConfigGroup, NewSkill, ProbeId, ProbeMetadata, Skill, SkillId, SkillLocation};
use psa_config::server::AgentDefaults;
use psa_store::{Database, SkillRegistration};
use std::{collections::HashMap, path::Path, path::PathBuf};

/// Discovers `.agents` directories and manages skill sources. Errors are
/// reported to clients as invalid requests, so they are plain messages.
pub trait ConfigDiscovery: Send + Sync + 'static {
    /// Defaults applied to agent configurations before they are saved.
    fn agent_defaults(&self) -> &AgentDefaults;

    /// Find the `.agents` directory governing `cwd` on a connected probe.
    fn discover_agents_dir<'a>(
        &'a self,
        probe: &'a ProbeHandle,
        probe_id: ProbeId,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Option<ConfigGroup>, String>>;

    /// Rescan global configuration and the given probe directories into the database.
    fn refresh<'a>(
        &'a self,
        database: &'a Database,
        agents_dirs: &'a [(ProbeHandle, ConfigGroup)],
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Store an uploaded skill's files, returning its registration.
    fn upload_skill<'a>(
        &'a self,
        upload: &'a NewSkill,
    ) -> BoxFuture<'a, Result<SkillRegistration, String>>;

    /// Delete the files of an upload that is no longer registered.
    fn remove_uploaded_source<'a>(
        &'a self,
        location: &'a SkillLocation,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// Copy the selected skills to a probe, returning where each landed.
    fn materialize_skills<'a>(
        &'a self,
        probe: &'a ProbeHandle,
        metadata: &'a ProbeMetadata,
        selected: &'a [Skill],
    ) -> BoxFuture<'a, Result<HashMap<SkillId, PathBuf>, String>>;
}
