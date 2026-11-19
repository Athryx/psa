//! Mutable source registry and lazy, cycle-aware immutable snapshot resolution.
use super::agent_definitions::CreateAgentDefinition;
use super::{
    DbError, ReadCursor, WriteCursor,
    schema::{decode_integer, encode_integer},
};
use crate::components::bundles::validate_completion_config;
use psa::api::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

type Models = HashMap<ConfigId, AgentModel>;

fn key(value: &impl Serialize) -> Result<String, DbError> {
    Ok(serde_json::to_string(value)?)
}

fn validate_id(id: &ConfigId) -> Result<(), DbError> {
    if id.name.trim().is_empty() || id.name.trim() != id.name {
        return Err(DbError::InvalidInput(
            "configuration name must be nonempty without surrounding whitespace".into(),
        ));
    }
    match &id.config_group {
        ConfigGroup::Custom(name) if name.trim().is_empty() => {
            return Err(DbError::InvalidInput(
                "custom group must be nonempty".into(),
            ));
        }
        ConfigGroup::AgentsDir { path, .. } if !std::path::Path::new(path).is_absolute() => {
            return Err(DbError::InvalidInput(
                "AgentsDir group path must be absolute".into(),
            ));
        }
        ConfigGroup::Session => {
            return Err(DbError::InvalidInput(
                "Session is only valid as an included group".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

fn validate_batch(agents: &[AgentConfig], skills: &[SkillRegistration]) -> Result<(), DbError> {
    let mut seen = HashSet::new();
    for config in agents {
        validate_id(&config.config_id)?;
        validate_completion_config(&config.bundles)
            .map_err(|error| DbError::InvalidInput(error.to_string()))?;
        if !seen.insert(&config.config_id) {
            return Err(DbError::InvalidInput(
                "duplicate agent config ID in batch".into(),
            ));
        }
        if config.models.is_empty() {
            return Err(DbError::InvalidInput(
                "agent models must not be empty".into(),
            ));
        }
        let mut model_names = HashSet::new();
        for model in &config.models {
            // Slugs always contain a slash, so they cannot collide with the keywords.
            let name = match model {
                ModelSelection::Fixed { model, .. } => model.slug(),
                ModelSelection::Global => "global".into(),
                ModelSelection::Parent => "parent".into(),
            };
            if !model_names.insert(name) {
                return Err(DbError::InvalidInput(
                    "only one selection and effort policy per model is allowed".into(),
                ));
            }
        }
        for id in config.subagents.items.iter().chain(&config.skills.items) {
            validate_id(id)?;
        }
    }
    seen.clear();
    for skill in skills {
        validate_id(&skill.config_id)?;
        if !seen.insert(&skill.config_id) {
            return Err(DbError::InvalidInput(
                "duplicate skill config ID in batch".into(),
            ));
        }
        if skill.name != skill.config_id.name {
            return Err(DbError::InvalidInput(
                "skill name must match config ID name".into(),
            ));
        }
        let path = match &skill.skill_location {
            SkillLocation::Global(path) | SkillLocation::Probe { path, .. } => path,
        };
        if !path.is_absolute() {
            return Err(DbError::InvalidInput(
                "skill source path must be absolute".into(),
            ));
        }
    }
    Ok(())
}

/// Group order wins by name; explicit items win last, even when missing.
/// Keeping a missing explicit override prevents silently falling back to a different source.
/// `ConfigGroup::Session` stands for `session_group` and selects nothing without one.
fn select_items(
    included: &IncludedItems,
    available: &[ConfigId],
    exclude: Option<&ConfigId>,
    session_group: Option<&ConfigGroup>,
) -> Vec<ConfigId> {
    let mut selected = BTreeMap::new();
    for group in &included.groups {
        let group = match group {
            ConfigGroup::Session => match session_group {
                Some(group) => group,
                None => continue,
            },
            group => group,
        };
        for id in available {
            if &id.config_group == group && Some(id) != exclude {
                selected.insert(id.name.clone(), id.clone());
            }
        }
    }
    for id in &included.items {
        selected.insert(id.name.clone(), id.clone());
    }
    selected.into_values().collect()
}

#[derive(Clone, Serialize)]
struct SelectedNode {
    config: AgentConfig,
    children: Vec<ConfigId>,
    skill_ids: Vec<SkillId>,
    skill_paths: Vec<PathBuf>,
    model: AgentModel,
}

impl ReadCursor {
    /// Source manifest for adapters calculating the catalogue after a successful scan.
    /// The tuple contains agent IDs and active skill IDs respectively.
    pub async fn list_discovered_config_ids(
        &mut self,
    ) -> Result<(Vec<ConfigId>, Vec<ConfigId>), DbError> {
        let agents: Vec<String> = sqlx::query_scalar(
            "SELECT config_key FROM agent_configs WHERE discovered=TRUE ORDER BY config_key",
        )
        .fetch_all(self.connection())
        .await?;
        let skills: Vec<String> = sqlx::query_scalar("SELECT config_key FROM registered_skills WHERE discovered=TRUE AND active=TRUE ORDER BY config_key")
            .fetch_all(self.connection()).await?;
        let decode = |ids: Vec<String>| -> Result<Vec<ConfigId>, DbError> {
            ids.into_iter()
                .map(|id| serde_json::from_str(&id).map_err(Into::into))
                .collect()
        };
        Ok((decode(agents)?, decode(skills)?))
    }

    async fn current_configs(&mut self) -> Result<Vec<AgentConfig>, DbError> {
        let rows = sqlx::query("SELECT payload FROM agent_configs ORDER BY config_key")
            .fetch_all(self.connection())
            .await?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(row.try_get("payload")?)?))
            .collect()
    }

    pub async fn list_skills(&mut self) -> Result<Vec<Skill>, DbError> {
        self.list_registered_skills(false).await
    }

    /// Includes inactive source tombstones so refresh adapters can retain a
    /// missing path's metadata-derived identity across repeated scans/restarts.
    /// These records must not be offered for selection or materialization.
    pub async fn list_skill_sources(&mut self) -> Result<Vec<Skill>, DbError> {
        self.list_registered_skills(true).await
    }

    async fn list_registered_skills(
        &mut self,
        include_inactive: bool,
    ) -> Result<Vec<Skill>, DbError> {
        let rows = sqlx::query(
            "SELECT id,payload FROM registered_skills WHERE (? OR active=TRUE) ORDER BY config_key",
        )
        .bind(include_inactive)
        .fetch_all(self.connection())
        .await?;
        rows.into_iter()
            .map(|row| {
                let input: SkillRegistration = serde_json::from_str(row.try_get("payload")?)?;
                Ok(Skill {
                    id: SkillId(decode_integer(row.try_get("id")?)?),
                    config_id: input.config_id,
                    name: input.name,
                    description: input.description,
                    skill_location: input.skill_location,
                    disable_model_invocation: input.disable_model_invocation,
                    archive_sha256: input.archive_sha256,
                })
            })
            .collect()
    }

    pub async fn get_skill(&mut self, id: SkillId) -> Result<Skill, DbError> {
        self.list_skills()
            .await?
            .into_iter()
            .find(|skill| skill.id == id)
            .ok_or(DbError::NotFound {
                entity: "skill",
                id: id.0,
            })
    }

    pub async fn list_agent_configs(&mut self) -> Result<Vec<AgentConfigStatus>, DbError> {
        let configs = self.current_configs().await?;
        let skills = self.list_skills().await?;
        let agents: Vec<_> = configs.iter().map(|item| item.config_id.clone()).collect();
        let skill_ids: Vec<_> = skills.iter().map(|item| item.config_id.clone()).collect();
        Ok(configs
            .into_iter()
            .map(|config| {
                let mut missing_components = Vec::new();
                for config_id in
                    select_items(&config.subagents, &agents, Some(&config.config_id), None)
                {
                    if !agents.contains(&config_id) {
                        missing_components.push(MissingComponent::Agent { config_id });
                    }
                }
                for config_id in select_items(&config.skills, &skill_ids, None, None) {
                    if !skill_ids.contains(&config_id) {
                        missing_components.push(MissingComponent::Skill { config_id });
                    }
                }
                AgentConfigStatus {
                    config,
                    status: AgentDefinitionStatus { missing_components },
                }
            })
            .collect())
    }

    pub async fn get_agent_config(&mut self, id: &ConfigId) -> Result<AgentConfigStatus, DbError> {
        self.list_agent_configs()
            .await?
            .into_iter()
            .find(|item| &item.config.config_id == id)
            .ok_or_else(|| DbError::ConfigNotFound(id.clone()))
    }

    /// `session_group` is the creating session's `.agents` group, if any.
    pub async fn get_agent_config_graph(
        &mut self,
        root: &ConfigId,
        session_group: Option<&ConfigGroup>,
    ) -> Result<AgentConfigGraph, DbError> {
        let statuses = self.list_agent_configs().await?;
        let all_skills = self.list_skills().await?;
        let agents: Vec<_> = statuses
            .iter()
            .map(|item| item.config.config_id.clone())
            .collect();
        let skill_ids: Vec<_> = all_skills
            .iter()
            .map(|item| item.config_id.clone())
            .collect();
        if !agents.contains(root) {
            return Err(DbError::ConfigNotFound(root.clone()));
        }
        let mut pending = vec![root.clone()];
        let mut seen = HashSet::new();
        let mut selected_skills = HashSet::new();
        let mut graph = AgentConfigGraph {
            configs: Vec::new(),
            skills: Vec::new(),
            statuses: Vec::new(),
        };
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(item) = statuses.iter().find(|item| item.config.config_id == id) else {
                continue;
            };
            pending.extend(select_items(
                &item.config.subagents,
                &agents,
                Some(&id),
                session_group,
            ));
            selected_skills.extend(select_items(
                &item.config.skills,
                &skill_ids,
                None,
                session_group,
            ));
            graph.configs.push(item.config.clone());
            graph.statuses.push(item.clone());
        }
        graph.skills = all_skills
            .into_iter()
            .filter(|skill| selected_skills.contains(&skill.config_id))
            .collect();
        Ok(graph)
    }

    /// None means a legacy definition without registry metadata.
    pub async fn get_resolved_agent_config(
        &mut self,
        id: AgentDefinitionId,
    ) -> Result<Option<ResolvedAgentConfig>, DbError> {
        let row =
            sqlx::query("SELECT config,skill_ids FROM config_snapshots WHERE definition_id=?")
                .bind(encode_integer(id.0)?)
                .fetch_optional(self.connection())
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let definition = self.get_agent_definition(id).await?;
        Ok(Some(ResolvedAgentConfig {
            config: serde_json::from_str(row.try_get("config")?)?,
            references: ResolvedDefinitionReferences {
                subagents: definition.permitted_subagents.clone(),
                skills: serde_json::from_str(row.try_get("skill_ids")?)?,
            },
            definition,
        }))
    }

    pub async fn has_session_agents_dir_discovery(
        &mut self,
        session: SessionId,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM session_agents_dirs WHERE session_id=?)",
        )
        .bind(encode_integer(session.0)?)
        .fetch_one(self.connection())
        .await?)
    }

    pub async fn get_session_agents_dir(
        &mut self,
        session: SessionId,
    ) -> Result<Option<ConfigGroup>, DbError> {
        let agents_dir: Option<String> =
            sqlx::query_scalar("SELECT config_group FROM session_agents_dirs WHERE session_id=?")
                .bind(encode_integer(session.0)?)
                .fetch_optional(self.connection())
                .await?;
        let agents_dir: Option<Option<ConfigGroup>> = agents_dir
            .map(|value| serde_json::from_str(&value).map_err(DbError::from))
            .transpose()?;
        Ok(agents_dir.flatten())
    }
}

impl WriteCursor {
    pub async fn upsert_agent_configs(
        &mut self,
        configs: Vec<AgentConfig>,
    ) -> Result<Vec<AgentConfigStatus>, DbError> {
        validate_batch(&configs, &[])?;
        self.write_registry(&configs, &[], false).await?;
        let all = self.list_agent_configs().await?;
        Ok(configs
            .iter()
            .filter_map(|config| {
                all.iter()
                    .find(|item| item.config.config_id == config.config_id)
                    .cloned()
            })
            .collect())
    }

    pub async fn upsert_skills(
        &mut self,
        skills: Vec<SkillRegistration>,
    ) -> Result<Vec<Skill>, DbError> {
        validate_batch(&[], &skills)?;
        self.write_registry(&[], &skills, false).await?;
        let all = self.list_skills().await?;
        Ok(skills
            .iter()
            .filter_map(|skill| {
                all.iter()
                    .find(|item| item.config_id == skill.config_id)
                    .cloned()
            })
            .collect())
    }

    async fn write_registry(
        &mut self,
        agents: &[AgentConfig],
        skills: &[SkillRegistration],
        discovered: bool,
    ) -> Result<(), DbError> {
        sqlx::query("SAVEPOINT registry_write")
            .execute(self.connection())
            .await?;
        let result = async {
            for agent in agents {
                let mut agent = agent.clone();
                if agent.models.len() == 1 {
                    agent.first_model_default = true;
                }
                sqlx::query("INSERT INTO agent_configs(config_key,config_group,payload,discovered) VALUES(?,?,?,?) ON CONFLICT(config_key) DO UPDATE SET payload=excluded.payload,discovered=excluded.discovered")
                    .bind(key(&agent.config_id)?).bind(key(&agent.config_id.config_group)?).bind(key(&agent)?).bind(discovered)
                    .execute(self.connection()).await?;
            }
            for skill in skills {
                sqlx::query("INSERT INTO registered_skills(config_key,config_group,payload,discovered) VALUES(?,?,?,?) ON CONFLICT(config_key) DO UPDATE SET payload=excluded.payload,active=TRUE,discovered=excluded.discovered")
                    .bind(key(&skill.config_id)?).bind(key(&skill.config_id.config_group)?).bind(key(skill)?).bind(discovered)
                    .execute(self.connection()).await?;
            }
            Ok::<_, DbError>(())
        }.await;
        self.finish_registry_savepoint(result).await
    }

    async fn finish_registry_savepoint<T>(
        &mut self,
        result: Result<T, DbError>,
    ) -> Result<T, DbError> {
        if result.is_err() {
            sqlx::query("ROLLBACK TO registry_write")
                .execute(self.connection())
                .await?;
        }
        sqlx::query("RELEASE registry_write")
            .execute(self.connection())
            .await?;
        result
    }

    /// Authoritative full-group replacement. Call only after a successful complete scan.
    pub async fn replace_config_group(
        &mut self,
        group: &ConfigGroup,
        agents: Vec<AgentConfig>,
        skills: Vec<SkillRegistration>,
    ) -> Result<(), DbError> {
        self.replace_registry_group(group, agents, skills, false)
            .await
    }

    /// Replace only discovery-owned records, preserving unrelated API registrations.
    pub async fn replace_discovered_group(
        &mut self,
        group: &ConfigGroup,
        agents: Vec<AgentConfig>,
        skills: Vec<SkillRegistration>,
    ) -> Result<(), DbError> {
        self.replace_registry_group(group, agents, skills, true)
            .await
    }

    async fn replace_registry_group(
        &mut self,
        group: &ConfigGroup,
        agents: Vec<AgentConfig>,
        skills: Vec<SkillRegistration>,
        discovered: bool,
    ) -> Result<(), DbError> {
        validate_batch(&agents, &skills)?;
        if agents
            .iter()
            .any(|item| &item.config_id.config_group != group)
            || skills
                .iter()
                .any(|item| &item.config_id.config_group != group)
        {
            return Err(DbError::InvalidInput(
                "replacement contains a different config group".into(),
            ));
        }
        sqlx::query("SAVEPOINT registry_write")
            .execute(self.connection())
            .await?;
        let result = async {
            let agent_keys: HashSet<_> = agents.iter().map(|item| key(&item.config_id)).collect::<Result<_, _>>()?;
            let skill_keys: HashSet<_> = skills.iter().map(|item| key(&item.config_id)).collect::<Result<_, _>>()?;
            let group_key = key(group)?;
            let rows = sqlx::query("SELECT config_key FROM agent_configs WHERE config_group=? AND (?=FALSE OR discovered=TRUE)")
                .bind(&group_key).bind(discovered).fetch_all(self.connection()).await?;
            for row in rows {
                let id: String = row.try_get("config_key")?;
                if !agent_keys.contains(&id) { sqlx::query("DELETE FROM agent_configs WHERE config_key=?").bind(id).execute(self.connection()).await?; }
            }
            let rows = sqlx::query("SELECT config_key FROM registered_skills WHERE config_group=? AND active=TRUE AND (?=FALSE OR discovered=TRUE)")
                .bind(&group_key).bind(discovered).fetch_all(self.connection()).await?;
            for row in rows {
                let id: String = row.try_get("config_key")?;
                if !skill_keys.contains(&id) { sqlx::query("UPDATE registered_skills SET active=FALSE WHERE config_key=?").bind(id).execute(self.connection()).await?; }
            }
            self.write_registry(&agents, &skills, discovered).await
        }.await;
        self.finish_registry_savepoint(result).await
    }

    /// All required copies must complete before calling this method. Missing path
    /// mappings are errors, not missing-component warnings. Omitted model entries
    /// follow the session model.
    pub async fn resolve_agent_config(
        &mut self,
        root: &ConfigId,
        session_group: Option<&ConfigGroup>,
        skill_paths: &HashMap<SkillId, PathBuf>,
        models: &Models,
    ) -> Result<ResolvedAgentConfig, DbError> {
        let graph = self.get_agent_config_graph(root, session_group).await?;
        let agents: Vec<_> = graph
            .configs
            .iter()
            .map(|item| item.config_id.clone())
            .collect();
        let skills: Vec<_> = graph
            .skills
            .iter()
            .map(|item| item.config_id.clone())
            .collect();
        let mut nodes = HashMap::new();
        for config in graph.configs {
            let children = select_items(
                &config.subagents,
                &agents,
                Some(&config.config_id),
                session_group,
            )
            .into_iter()
            .filter(|id| agents.contains(id))
            .collect();
            let mut ids = Vec::new();
            let mut paths = Vec::new();
            for id in select_items(&config.skills, &skills, None, session_group) {
                let Some(skill) = graph.skills.iter().find(|item| item.config_id == id) else {
                    continue;
                };
                ids.push(skill.id);
                paths.push(
                    skill_paths
                        .get(&skill.id)
                        .ok_or_else(|| {
                            DbError::InvalidInput(format!(
                                "skill {} has not been materialized",
                                skill.id.0
                            ))
                        })?
                        .clone(),
                );
            }
            let node = SelectedNode {
                model: models.get(&config.config_id).cloned().unwrap_or_default(),
                config,
                children,
                skill_ids: ids,
                skill_paths: paths,
            };
            nodes.insert(node.config.config_id.clone(), node);
        }
        let mut signatures = HashMap::new();
        let mut resolved = HashMap::new();
        for id in &agents {
            let signature = graph_signature(id, &nodes)?;
            let cached: Option<i64> = sqlx::query_scalar(
                "SELECT definition_id FROM config_snapshot_cache WHERE signature=?",
            )
            .bind(&signature)
            .fetch_optional(self.connection())
            .await?;
            if let Some(cached) = cached {
                resolved.insert(id.clone(), AgentDefinitionId(decode_integer(cached)?));
            }
            signatures.insert(id.clone(), signature);
        }
        // Allocate every unresolved node before filling any edges (including cycles).
        sqlx::query("SAVEPOINT registry_write")
            .execute(self.connection())
            .await?;
        let result = async {
            let mut fresh = Vec::new();
            for id in &agents {
                if resolved.contains_key(id) {
                    continue;
                }
                let node = &nodes[id];
                let definition = self
                    .create_agent_definition(snapshot_input(node, Vec::new()))
                    .await?;
                resolved.insert(id.clone(), definition.id);
                fresh.push(id.clone());
            }
            for id in fresh {
                let node = &nodes[&id];
                let definition_id = resolved[&id];
                let children: Vec<_> = node.children.iter().map(|id| resolved[id]).collect();
                sqlx::query("UPDATE agent_definitions SET permitted_subagents=? WHERE id=?")
                    .bind(key(&children)?)
                    .bind(encode_integer(definition_id.0)?)
                    .execute(self.connection())
                    .await?;
                self.store_snapshot_metadata(definition_id, &node.config, &node.skill_ids)
                    .await?;
                sqlx::query(
                    "INSERT INTO config_snapshot_cache(signature,definition_id) VALUES(?,?)",
                )
                .bind(&signatures[&id])
                .bind(encode_integer(definition_id.0)?)
                .execute(self.connection())
                .await?;
            }
            self.get_resolved_agent_config(resolved[root])
                .await?
                .ok_or_else(|| DbError::Corrupt("resolved snapshot metadata absent".into()))
        }
        .await;
        self.finish_registry_savepoint(result).await
    }

    async fn store_snapshot_metadata(
        &mut self,
        id: AgentDefinitionId,
        config: &AgentConfig,
        skills: &[SkillId],
    ) -> Result<(), DbError> {
        sqlx::query("INSERT INTO config_snapshots(definition_id,config,skill_ids) VALUES(?,?,?)")
            .bind(encode_integer(id.0)?)
            .bind(key(config)?)
            .bind(serde_json::to_string(skills)?)
            .execute(self.connection())
            .await?;
        Ok(())
    }

    /// Select a creation-time model without consulting mutable current configs or
    /// advancing any pinned subagent edges. Existing snapshots remain unchanged.
    pub async fn resolve_pinned_agent_config(
        &mut self,
        id: AgentDefinitionId,
        model: AgentModel,
        skill_paths: &HashMap<SkillId, PathBuf>,
    ) -> Result<ResolvedAgentConfig, DbError> {
        let mut pinned = self.get_resolved_agent_config(id).await?.ok_or_else(|| {
            DbError::InvalidInput("definition has no configuration metadata".into())
        })?;
        let mut retained = Vec::new();
        let mut paths = Vec::new();
        let active = self.list_skills().await?;
        for skill in &pinned.references.skills {
            if !active.iter().any(|item| item.id == *skill) {
                continue;
            }
            let path = skill_paths.get(skill).ok_or_else(|| {
                DbError::InvalidInput(format!("skill {} has not been materialized", skill.0))
            })?;
            retained.push(*skill);
            paths.push(path.clone());
        }
        if pinned.definition.model == model
            && pinned.definition.skills == paths
            && pinned.references.skills == retained
        {
            return Ok(pinned);
        }
        pinned.references.skills = retained;
        pinned.definition.skills = paths;
        pinned.definition.model = model;
        let input = CreateAgentDefinition {
            name: pinned.definition.name.clone(),
            description: pinned.definition.description.clone(),
            selectable: pinned.definition.selectable,
            bundles: pinned.definition.bundles.clone(),
            model: pinned.definition.model.clone(),
            skills: pinned.definition.skills.clone(),
            permitted_subagents: pinned.definition.permitted_subagents.clone(),
        };
        let signature = format!(
            "pinned:{}",
            key(&(
                semantic_config(&pinned.config),
                &input,
                &pinned.references.skills
            ))?
        );
        let cached: Option<i64> =
            sqlx::query_scalar("SELECT definition_id FROM config_snapshot_cache WHERE signature=?")
                .bind(&signature)
                .fetch_optional(self.connection())
                .await?;
        if let Some(cached) = cached {
            return self
                .get_resolved_agent_config(AgentDefinitionId(decode_integer(cached)?))
                .await?
                .ok_or_else(|| DbError::Corrupt("cached snapshot absent".into()));
        }
        sqlx::query("SAVEPOINT registry_write")
            .execute(self.connection())
            .await?;
        let result = async {
            let definition = self.create_agent_definition(input).await?;
            self.store_snapshot_metadata(definition.id, &pinned.config, &pinned.references.skills)
                .await?;
            sqlx::query("INSERT INTO config_snapshot_cache(signature,definition_id) VALUES(?,?)")
                .bind(signature)
                .bind(encode_integer(definition.id.0)?)
                .execute(self.connection())
                .await?;
            pinned.definition = definition;
            Ok(pinned)
        }
        .await;
        self.finish_registry_savepoint(result).await
    }

    pub async fn set_session_agents_dir(
        &mut self,
        session: SessionId,
        agents_dir: Option<ConfigGroup>,
    ) -> Result<(), DbError> {
        if agents_dir
            .as_ref()
            .is_some_and(|group| !matches!(group, ConfigGroup::AgentsDir { .. }))
        {
            return Err(DbError::InvalidInput(
                "a session agents directory must be an AgentsDir group".into(),
            ));
        }
        // JSON null records a successful scan that found no `.agents` directory. An absent
        // row means discovery is deferred (for example, the probe was offline).
        sqlx::query("INSERT INTO session_agents_dirs(session_id,config_group) VALUES(?,?) ON CONFLICT(session_id) DO UPDATE SET config_group=excluded.config_group")
            .bind(encode_integer(session.0)?).bind(key(&agents_dir)?).execute(self.connection()).await?;
        Ok(())
    }

    /// Mark discovery unknown, rather than a successful scan that found nothing.
    pub async fn clear_session_agents_dir(&mut self, session: SessionId) -> Result<(), DbError> {
        sqlx::query("DELETE FROM session_agents_dirs WHERE session_id=?")
            .bind(encode_integer(session.0)?)
            .execute(self.connection())
            .await?;
        Ok(())
    }
}

fn snapshot_input(node: &SelectedNode, children: Vec<AgentDefinitionId>) -> CreateAgentDefinition {
    CreateAgentDefinition {
        name: node.config.config_id.name.clone(),
        description: node.config.description.clone(),
        selectable: node.config.selectable,
        bundles: node.config.bundles.clone(),
        model: node.model.clone(),
        skills: node.skill_paths.clone(),
        permitted_subagents: children,
    }
}

fn semantic_config(config: &AgentConfig) -> AgentConfig {
    let mut config = config.clone();
    config.subagents = IncludedItems::default();
    config.skills = IncludedItems::default();
    config
}

/// Canonical reachable graph identity, expressed in stable config IDs rather
/// than newly allocated snapshot IDs. This handles cycles without recursion or
/// provisional public rows and reuses unaffected downstream subgraphs.
fn graph_signature(
    root: &ConfigId,
    nodes: &HashMap<ConfigId, SelectedNode>,
) -> Result<String, DbError> {
    let mut pending = vec![root.clone()];
    let mut selected = BTreeMap::new();
    while let Some(id) = pending.pop() {
        let encoded = key(&id)?;
        if selected.contains_key(&encoded) {
            continue;
        }
        let mut node = nodes[&id].clone();
        pending.extend(node.children.clone());
        node.config = semantic_config(&node.config);
        selected.insert(encoded, node);
    }
    key(&(root, selected))
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
    /// SHA-256 of the uploaded archive; absent for discovered skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
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
mod tests;
