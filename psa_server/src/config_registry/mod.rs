//! Async file discovery adapters for the shared configuration registry.
//! A failed scan never publishes a partial batch or deletes previous records.
pub mod skills;
#[cfg(test)]
mod tests;
mod transfer;
mod upload;

use crate::{
    db::{Database, DbError},
    probes::ProbeHandle,
};
use psa_api::probes::{DirectoryEntry, Event};
use psa_api::{
    AgentConfig, AgentConfigInput, AgentDefaults, BundleId, ConfigGroup, ConfigId, HookOverrides,
    IncludedItems, ModelSelection, ProbeId, Skill, SkillId, SkillLocation, SkillRegistration,
    ToolId,
};
use psa_shared::probe_transport::ProbeError;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("{0}")]
    Invalid(String),
    #[error("cannot {operation} {path}: {message}")]
    Source {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
}

/// Clones share refresh/publication serialization, but no long-lived task is needed.
#[derive(Clone)]
pub struct ConfigRegistry {
    data_dir: PathBuf,
    agent_defaults: AgentDefaults,
    gate: Arc<Mutex<()>>,
}

impl ConfigRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            agent_defaults: AgentDefaults::default(),
            gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_agent_defaults(mut self, agent_defaults: AgentDefaults) -> Self {
        self.agent_defaults = agent_defaults;
        self
    }

    pub fn agent_defaults(&self) -> &AgentDefaults {
        &self.agent_defaults
    }

    /// Select exactly the nearest .agents directory, even when it is empty.
    pub async fn discover_project(
        &self,
        probe: &ProbeHandle,
        probe_id: ProbeId,
        cwd: &Path,
    ) -> Result<Option<ConfigGroup>, DiscoveryError> {
        let mut current = normalize_absolute(cwd)?;
        loop {
            let entries = probe.list_dir(&current).await?;
            if let Some(entry) = entries.iter().find(|entry| entry.name == ".agents") {
                if entry.kind != "directory" {
                    return Err(DiscoveryError::Invalid(format!(
                        "{} is not a directory",
                        current.join(".agents").display()
                    )));
                }
                return Ok(Some(ConfigGroup::Project {
                    path: current.join(".agents").to_string_lossy().into_owned(),
                    probe_id,
                }));
            }
            if !current.pop() {
                return Ok(None);
            }
        }
    }

    /// Refresh globals and optionally the already-selected project. Never rediscovers ancestors.
    pub async fn refresh(
        &self,
        database: &Database,
        project: Option<(&ProbeHandle, &ConfigGroup)>,
    ) -> Result<(), DiscoveryError> {
        let _guard = self.gate.lock().await;
        tokio::fs::create_dir_all(&self.data_dir).await?;
        let global_root = tokio::fs::canonicalize(&self.data_dir).await?;
        let mut cursor = database.cursor().await?;
        let previous_skills = cursor.list_skill_sources().await?;
        cursor.rollback().await?;
        let mut batches = vec![
            scan_group(
                Source::Local,
                global_root,
                ConfigGroup::Global,
                &previous_skills,
            )
            .await?,
        ];
        if let Some((probe, group)) = project {
            let ConfigGroup::Project { path, .. } = group else {
                return Err(DiscoveryError::Invalid(
                    "selected project must have a project config group".into(),
                ));
            };
            // A successfully observed missing selected project becomes an empty
            // source, never an invitation to discover a different ancestor.
            batches.push(
                scan_group(
                    Source::Probe(probe),
                    PathBuf::from(path),
                    group.clone(),
                    &previous_skills,
                )
                .await?,
            );
        }

        let mut cursor = database.cursor().await?;
        let existing_agents = cursor.list_agent_configs().await?;
        let existing_skills = cursor.list_skills().await?;
        let (discovered_agents, discovered_skills) = cursor.list_discovered_config_ids().await?;
        let discovered_agents: HashSet<_> = discovered_agents.into_iter().collect();
        let discovered_skills: HashSet<_> = discovered_skills.into_iter().collect();
        let refreshed_groups: HashSet<_> =
            batches.iter().map(|batch| batch.group.clone()).collect();
        // Uploaded/API-owned server directories do not belong to a discovery
        // source batch. Refresh their metadata without acquiring discovery ownership.
        let mut registered_skills = Vec::new();
        for skill in &existing_skills {
            if discovered_skills.contains(&skill.config_id) {
                continue;
            }
            let SkillLocation::Global(path) = &skill.skill_location else {
                continue;
            };
            let file = path.join("SKILL.md");
            let parsed = skills::parse_skill(&file, &Source::Local.read(&file).await?).map_err(
                |message| DiscoveryError::Invalid(format!("{}: {message}", file.display())),
            )?;
            if parsed.name != skill.config_id.name {
                return Err(DiscoveryError::Invalid(format!(
                    "{}: metadata name must match registered config ID",
                    file.display()
                )));
            }
            registered_skills.push(SkillRegistration {
                config_id: skill.config_id.clone(),
                name: parsed.name,
                description: parsed.description,
                skill_location: skill.skill_location.clone(),
                disable_model_invocation: parsed.disable_model_invocation,
            });
        }
        let mut agent_ids: HashSet<_> = existing_agents
            .into_iter()
            .map(|status| status.config.config_id)
            .filter(|id| {
                !refreshed_groups.contains(&id.config_group) || !discovered_agents.contains(id)
            })
            .collect();
        let mut skill_ids: HashSet<_> = existing_skills
            .into_iter()
            .map(|skill| skill.config_id)
            .filter(|id| {
                !refreshed_groups.contains(&id.config_group) || !discovered_skills.contains(id)
            })
            .collect();
        for batch in &batches {
            agent_ids.extend(batch.agents.iter().map(|(_, config, _)| ConfigId {
                config_group: batch.group.clone(),
                name: config.name.clone(),
            }));
            skill_ids.extend(batch.skills.iter().map(|skill| skill.config_id.clone()));
        }
        cursor.upsert_skills(registered_skills).await?;
        for batch in batches {
            let agents = batch
                .agents
                .into_iter()
                .map(|(_, config, additional)| {
                    config.lower(
                        &batch.group,
                        project.map(|(_, group)| group),
                        additional,
                        &agent_ids,
                        &skill_ids,
                        &self.agent_defaults,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            cursor
                .replace_discovered_group(&batch.group, agents, batch.skills)
                .await?;
        }
        cursor.commit().await?;
        Ok(())
    }

    /// Return SKILL.md paths only after every required server-owned copy is published.
    pub async fn materialize_skills(
        &self,
        probe: &ProbeHandle,
        probe_id: ProbeId,
        probe_data_dir: &Path,
        selected: &[Skill],
    ) -> Result<HashMap<SkillId, PathBuf>, DiscoveryError> {
        let _guard = self.gate.lock().await;
        let mut paths = HashMap::new();
        for skill in selected {
            let directory = match &skill.skill_location {
                SkillLocation::Probe { id, path } => {
                    if *id != probe_id {
                        return Err(DiscoveryError::Invalid(format!(
                            "skill {} belongs to probe {id}, not {probe_id}",
                            skill.name
                        )));
                    }
                    path.clone()
                }
                SkillLocation::Global(source) => {
                    let destination = normalize_absolute(probe_data_dir)?
                        .join("skill-cache")
                        .join(skill.id.0.to_string());
                    transfer::sync_directory(probe, source, &destination).await?
                }
            };
            paths.insert(skill.id, directory.join("SKILL.md"));
        }
        Ok(paths)
    }
}

#[derive(Clone, Copy)]
enum Source<'a> {
    Local,
    Probe(&'a ProbeHandle),
}

impl Source<'_> {
    /// Absence must be observed successfully, never inferred from a failed read.
    async fn exists(self, path: &Path) -> Result<bool, DiscoveryError> {
        match self {
            Self::Local => match tokio::fs::symlink_metadata(path).await {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            },
            Self::Probe(probe) => {
                let path = normalize_absolute(path)?;
                // The ordinary case needs only the immediate parent. A remote
                // operation error may mean a missing intermediate directory;
                // establish that through successful ancestor listings below.
                // Transport failures are never evidence of absence.
                if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                    match probe.list_dir(parent).await {
                        Ok(entries) => {
                            return Ok(entries
                                .iter()
                                .any(|entry| entry.name == name.to_string_lossy()));
                        }
                        Err(ProbeError::Remote { .. }) => {}
                        Err(error) => return Err(source_error("list directory", parent, error)),
                    }
                }
                let mut current = PathBuf::from("/");
                let components: Vec<_> = path
                    .components()
                    .filter_map(|part| match part {
                        Component::Normal(name) => Some(name),
                        _ => None,
                    })
                    .collect();
                for (index, component) in components.iter().enumerate() {
                    let entries = self.list(&current).await?;
                    let Some(entry) = entries
                        .iter()
                        .find(|entry| entry.name == component.to_string_lossy())
                    else {
                        return Ok(false);
                    };
                    if index + 1 < components.len() && entry.kind != "directory" {
                        return Err(DiscoveryError::Invalid(format!(
                            "{} is not a directory",
                            current.join(component).display()
                        )));
                    }
                    current.push(component);
                }
                Ok(true)
            }
        }
    }

    async fn list(self, path: &Path) -> Result<Vec<DirectoryEntry>, DiscoveryError> {
        match self {
            Self::Probe(probe) => probe
                .list_dir(path)
                .await
                .map_err(|error| source_error("list directory", path, error)),
            Self::Local => {
                let mut directory = tokio::fs::read_dir(path)
                    .await
                    .map_err(|error| source_error("list directory", path, error))?;
                let mut entries = Vec::new();
                while let Some(entry) = directory
                    .next_entry()
                    .await
                    .map_err(|error| source_error("list directory", path, error))?
                {
                    let metadata = tokio::fs::symlink_metadata(entry.path())
                        .await
                        .map_err(|error| source_error("inspect entry", &entry.path(), error))?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| DiscoveryError::Invalid("non UTF-8 config filename".into()))?;
                    entries.push(DirectoryEntry {
                        name,
                        kind: if metadata.is_dir() {
                            "directory"
                        } else if metadata.is_file() {
                            "file"
                        } else {
                            "other"
                        }
                        .into(),
                        size: metadata.len(),
                    });
                }
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(entries)
            }
        }
    }

    async fn read(self, path: &Path) -> Result<Vec<u8>, DiscoveryError> {
        use tokio::io::AsyncReadExt;
        match self {
            Self::Local => {
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|error| source_error("read file", path, error))?;
                let mut bytes = Vec::new();
                file.take(MAX_CONFIG_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| source_error("read file", path, error))?;
                if bytes.len() > MAX_CONFIG_BYTES {
                    return Err(DiscoveryError::Invalid(format!(
                        "{} exceeds 1 MiB",
                        path.display()
                    )));
                }
                Ok(bytes)
            }
            Self::Probe(probe) => {
                let mut call = probe
                    .read_file(path)
                    .await
                    .map_err(|error| source_error("read file", path, error))?;
                let mut bytes = Vec::new();
                let mut eof = false;
                loop {
                    match call
                        .next_event()
                        .await
                        .map_err(|error| source_error("read file", path, error))?
                    {
                        Event::Data {
                            stream: 1,
                            bytes: chunk,
                        } if !eof => {
                            if chunk.len() > MAX_CONFIG_BYTES - bytes.len() {
                                return Err(DiscoveryError::Invalid(format!(
                                    "{} exceeds 1 MiB",
                                    path.display()
                                )));
                            }
                            bytes.extend(chunk);
                        }
                        Event::Eof { stream: 1 } if !eof => eof = true,
                        Event::Complete { status: None } if eof => return Ok(bytes),
                        _ => {
                            return Err(DiscoveryError::Invalid(
                                "unexpected config read response".into(),
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn source_error(
    operation: &'static str,
    path: &Path,
    error: impl std::fmt::Display,
) -> DiscoveryError {
    DiscoveryError::Source {
        operation,
        path: path.to_owned(),
        message: error.to_string(),
    }
}

struct SourceBatch {
    group: ConfigGroup,
    agents: Vec<(PathBuf, AgentDefinitionConfig, Vec<ConfigId>)>,
    skills: Vec<SkillRegistration>,
}

async fn scan_group(
    source: Source<'_>,
    root: PathBuf,
    group: ConfigGroup,
    previous_skills: &[Skill],
) -> Result<SourceBatch, DiscoveryError> {
    let mut batch = SourceBatch {
        group: group.clone(),
        agents: Vec::new(),
        skills: Vec::new(),
    };
    if !source.exists(&root).await? {
        return Ok(batch);
    }
    let entries = source.list(&root).await?;
    let mut skill_paths = HashMap::<String, PathBuf>::new();
    for kind in ["skills", "agents"] {
        let Some(entry) = entries.iter().find(|entry| entry.name == kind) else {
            continue;
        };
        if entry.kind != "directory" {
            return Err(DiscoveryError::Invalid(format!(
                "{} is not a directory",
                root.join(kind).display()
            )));
        }
        let directory = root.join(kind);
        for entry in source.list(&directory).await? {
            let path = directory.join(&entry.name);
            if kind == "skills" && entry.kind == "directory" {
                if !source.exists(&path.join("SKILL.md")).await? {
                    continue;
                }
                add_skill(source, &group, path, &mut batch.skills, &mut skill_paths).await?;
            } else if kind == "agents"
                && entry.kind == "file"
                && path.extension().is_some_and(|ext| ext == "toml")
            {
                let bytes = source.read(&path).await?;
                let text = std::str::from_utf8(&bytes).map_err(|error| {
                    DiscoveryError::Invalid(format!("{}: {error}", path.display()))
                })?;
                let config: AgentDefinitionConfig = toml::from_str(text).map_err(|error| {
                    DiscoveryError::Invalid(format!("{}: {error}", path.display()))
                })?;
                if config.name.trim().is_empty() || config.models.is_empty() {
                    return Err(DiscoveryError::Invalid(format!(
                        "{}: name and models must not be empty",
                        path.display()
                    )));
                }
                if batch
                    .agents
                    .iter()
                    .any(|(_, previous, _)| previous.name == config.name)
                {
                    return Err(DiscoveryError::Invalid(format!(
                        "duplicate agent {} in group",
                        config.name
                    )));
                }
                batch.agents.push((path, config, Vec::new()));
            }
        }
    }
    for (file, config, additional) in &mut batch.agents {
        for path in &config.skills.additional_paths {
            let path = normalize_absolute(&file.parent().unwrap().join(path))?;
            if !source.exists(&path.join("SKILL.md")).await? {
                let prior = previous_skills.iter().find(|skill| {
                    if &skill.config_id.config_group != &group {
                        return false;
                    }
                    match &skill.skill_location {
                        SkillLocation::Global(previous)
                        | SkillLocation::Probe { path: previous, .. } => previous == &path,
                    }
                });
                let id = prior
                    .map(|skill| skill.config_id.clone())
                    .unwrap_or_else(|| ConfigId {
                        config_group: group.clone(),
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    });
                additional.push(id);
                continue;
            }
            let id = add_skill(source, &group, path, &mut batch.skills, &mut skill_paths).await?;
            additional.push(id);
        }
    }
    Ok(batch)
}

async fn add_skill(
    source: Source<'_>,
    group: &ConfigGroup,
    path: PathBuf,
    skills: &mut Vec<SkillRegistration>,
    paths: &mut HashMap<String, PathBuf>,
) -> Result<ConfigId, DiscoveryError> {
    let file = path.join("SKILL.md");
    let parsed = skills::parse_skill(&file, &source.read(&file).await?)
        .map_err(|message| DiscoveryError::Invalid(format!("{}: {message}", file.display())))?;
    let config_id = ConfigId {
        config_group: group.clone(),
        name: parsed.name.clone(),
    };
    if let Some(previous) = paths.get(&parsed.name) {
        if previous == &path {
            return Ok(config_id);
        }
        return Err(DiscoveryError::Invalid(format!(
            "duplicate skill {} in group",
            parsed.name
        )));
    }
    paths.insert(parsed.name.clone(), path.clone());
    let skill_location = match source {
        Source::Local => SkillLocation::Global(path),
        Source::Probe(_) => {
            let ConfigGroup::Project { probe_id, .. } = group else {
                return Err(DiscoveryError::Invalid(
                    "remote source requires a project group".into(),
                ));
            };
            SkillLocation::Probe {
                id: *probe_id,
                path,
            }
        }
    };
    skills.push(SkillRegistration {
        config_id: config_id.clone(),
        name: parsed.name,
        description: parsed.description,
        skill_location,
        disable_model_invocation: parsed.disable_model_invocation,
    });
    Ok(config_id)
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
    pub bundles: Option<Vec<BundleId>>,
    #[serde(default)]
    pub tools: Option<Vec<ToolId>>,
    #[serde(default)]
    pub hooks: HookOverrides,
    #[serde(default)]
    pub extra_system_prompt: String,
    #[serde(default = "default_models")]
    pub models: Vec<ModelSelection>,
    #[serde(default = "yes")]
    pub default: bool,
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
    pub project_defaults: bool,
    pub subagents: Vec<String>,
}
impl Default for SubagentInclusionsConfig {
    fn default() -> Self {
        Self {
            global_defaults: true,
            project_defaults: true,
            subagents: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillInclusionsConfig {
    pub global_defaults: bool,
    pub project_defaults: bool,
    pub skills: Vec<String>,
    pub additional_paths: Vec<PathBuf>,
}
impl Default for SkillInclusionsConfig {
    fn default() -> Self {
        Self {
            global_defaults: true,
            project_defaults: true,
            skills: Vec::new(),
            additional_paths: Vec::new(),
        }
    }
}

impl AgentDefinitionConfig {
    fn lower(
        self,
        group: &ConfigGroup,
        project: Option<&ConfigGroup>,
        additional: Vec<ConfigId>,
        agents: &HashSet<ConfigId>,
        skills: &HashSet<ConfigId>,
        defaults: &AgentDefaults,
    ) -> Result<AgentConfig, DiscoveryError> {
        let subagents = expand_inclusions(
            self.subagents.global_defaults,
            self.subagents.project_defaults,
            &self.subagents.subagents,
            group,
            project,
            agents,
        );
        let mut included_skills = expand_inclusions(
            self.skills.global_defaults,
            self.skills.project_defaults,
            &self.skills.skills,
            group,
            project,
            skills,
        );
        included_skills.items.extend(additional);
        Ok(AgentConfigInput {
            config_id: ConfigId {
                config_group: group.clone(),
                name: self.name,
            },
            description: self.description,
            selectable: self.selectable,
            bundles: self.bundles,
            tools: self.tools,
            hooks: self.hooks,
            extra_system_prompt: self.extra_system_prompt,
            models: self.models,
            default: self.default,
            subagents,
            skills: included_skills,
        }
        .resolve(defaults))
    }
}

fn expand_inclusions(
    global: bool,
    include_project: bool,
    names: &[String],
    current: &ConfigGroup,
    project: Option<&ConfigGroup>,
    available: &HashSet<ConfigId>,
) -> IncludedItems {
    let mut groups = Vec::new();
    if global {
        groups.push(ConfigGroup::Global);
    }
    if include_project && let Some(project) = project {
        groups.push(project.clone());
    }
    let items = names
        .iter()
        .map(|name| {
            let local = ConfigId {
                config_group: project.unwrap_or(current).clone(),
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

fn normalize_absolute(path: &Path) -> Result<PathBuf, DiscoveryError> {
    if !path.is_absolute() {
        return Err(DiscoveryError::Invalid(format!(
            "path must be absolute: {}",
            path.display()
        )));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                output.pop();
            }
            Component::CurDir => {}
            other => output.push(other.as_os_str()),
        }
    }
    Ok(output)
}
