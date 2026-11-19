//! Named configuration refresh, materialization, and creation-time snapshots.
use super::*;
use psa::api::{
    AgentConfig, AgentEffort, AgentModel, ConfigGroup, ConfigId, EffortSelection, ModelSelection,
};
use std::{collections::HashMap, path::Path};

#[cfg(test)]
#[path = "configurations_tests.rs"]
mod tests;

fn invalid(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Invalid(error.to_string())
}

impl RuntimeHandle {
    /// `Some(None)` is a completed scan that found no `.agents` directory;
    /// `None` defers discovery because the probe is offline.
    pub(super) async fn discover_session_agents_dir(
        &self,
        probe_id: api::ProbeId,
        cwd: &Path,
    ) -> Result<Option<Option<ConfigGroup>>, RuntimeError> {
        let Some(configuration) = &self.configuration else {
            return Ok(Some(None));
        };
        let probes = self.probes.as_ref().ok_or(RuntimeError::Unavailable)?;
        // Offline sessions remain valid; only connected probes can be discovered.
        if !probes.is_active(probe_id).await {
            return Ok(None);
        }
        let probe = probes.handle(probe_id).await.map_err(invalid)?;
        configuration
            .discover_agents_dir(&probe, probe_id, cwd)
            .await
            .map(Some)
            .map_err(invalid)
    }

    /// The session's `.agents` directory, discovering it on first use.
    async fn ensure_session_agents_dir(
        &self,
        id: SessionId,
    ) -> Result<Option<ConfigGroup>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let session = cursor.get_session(id).await?;
        let known = cursor.has_session_agents_dir_discovery(id).await?;
        let agents_dir = cursor.get_session_agents_dir(id).await?;
        cursor.rollback().await?;
        if known {
            return Ok(agents_dir);
        }

        let Some(discovered) = self
            .discover_session_agents_dir(session.probe_id, &session.working_directory)
            .await?
        else {
            return Ok(agents_dir);
        };
        let mut cursor = self.database.cursor().await?;
        cursor
            .set_session_agents_dir(id, discovered.clone())
            .await?;
        cursor.commit().await?;
        Ok(discovered)
    }

    /// Rescan globals and each distinct `.agents` directory of the sessions.
    /// Callers hold `configuration_gate`.
    async fn refresh_configuration(&self, sessions: &[SessionId]) -> Result<(), RuntimeError> {
        let Some(configuration) = &self.configuration else {
            return Ok(());
        };
        let mut agents_dirs = Vec::new();
        for &id in sessions {
            let Some(group) = self.ensure_session_agents_dir(id).await? else {
                continue;
            };
            let ConfigGroup::AgentsDir { probe_id, .. } = &group else {
                continue;
            };
            if agents_dirs.iter().any(|(_, known)| known == &group) {
                continue;
            }
            let probe = self
                .probes
                .as_ref()
                .ok_or(RuntimeError::Unavailable)?
                .handle(*probe_id)
                .await
                .map_err(invalid)?;
            agents_dirs.push((probe, group));
        }
        configuration
            .refresh(&self.database, &agents_dirs)
            .await
            .map_err(invalid)
    }

    pub async fn refresh_configs(&self, sessions: &[SessionId]) -> Result<(), RuntimeError> {
        let _guard = self.configuration_gate.write().await;
        self.refresh_configuration(sessions).await
    }

    pub async fn upsert_agent_configs<I: Into<api::AgentConfigInput>>(
        &self,
        configs: Vec<I>,
    ) -> Result<Vec<api::AgentConfigStatus>, RuntimeError> {
        let _guard = self.configuration_gate.write().await;
        let defaults = self
            .configuration
            .as_ref()
            .map(|configuration| configuration.agent_defaults().clone())
            .unwrap_or_default();
        let configs = configs
            .into_iter()
            .map(|config| defaults.resolve(config.into()))
            .collect();
        let mut cursor = self.database.cursor().await?;
        let configs = cursor.upsert_agent_configs(configs).await?;
        cursor.commit().await?;
        Ok(configs)
    }

    pub async fn list_agent_configs(&self) -> Result<Vec<api::AgentConfigStatus>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let configs = cursor.list_agent_configs().await?;
        cursor.rollback().await?;
        Ok(configs)
    }

    pub async fn get_agent_config(
        &self,
        id: ConfigId,
    ) -> Result<api::AgentConfigStatus, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let config = cursor.get_agent_config(&id).await?;
        cursor.rollback().await?;
        Ok(config)
    }

    pub async fn get_resolved_agent_config(
        &self,
        id: api::AgentDefinitionId,
    ) -> Result<Option<api::ResolvedAgentConfig>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let config = cursor.get_resolved_agent_config(id).await?;
        cursor.rollback().await?;
        Ok(config)
    }

    pub async fn upload_skill(&self, request: api::NewSkill) -> Result<api::Skill, RuntimeError> {
        let _guard = self.configuration_gate.write().await;
        let configuration = self
            .configuration
            .as_ref()
            .ok_or(RuntimeError::Unavailable)?;
        let mut cursor = self.database.cursor().await?;
        let previous = cursor
            .list_skill_sources()
            .await?
            .into_iter()
            .find(|skill| skill.config_id == request.config_id)
            .map(|skill| skill.skill_location);
        cursor.rollback().await?;

        let registration = configuration
            .upload_skill(&request)
            .await
            .map_err(invalid)?;
        let location = registration.skill_location.clone();
        let result = async {
            let mut cursor = self.database.cursor().await?;
            let mut skills = cursor.upsert_skills(vec![registration]).await?;
            cursor.commit().await?;
            Ok::<_, RuntimeError>(skills.remove(0))
        }
        .await;
        let obsolete = if result.is_ok() {
            previous
        } else {
            Some(location)
        };
        if let Some(obsolete) = obsolete {
            if let Err(error) = configuration.remove_uploaded_source(&obsolete).await {
                tracing::warn!(%error, "could not clean obsolete skill upload");
            }
        }
        result
    }

    pub async fn list_skills(&self) -> Result<Vec<api::Skill>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let skills = cursor.list_skills().await?;
        cursor.rollback().await?;
        Ok(skills)
    }

    async fn materialize_skills(
        &self,
        session: &sessions::Session,
        skills: &[api::Skill],
    ) -> Result<HashMap<api::SkillId, std::path::PathBuf>, RuntimeError> {
        if skills.is_empty() {
            return Ok(HashMap::new());
        }
        let configuration = self
            .configuration
            .as_ref()
            .ok_or(RuntimeError::Unavailable)?;
        let probe = self
            .probes
            .as_ref()
            .ok_or(RuntimeError::Unavailable)?
            .handle(session.probe_id)
            .await
            .map_err(invalid)?;

        let mut cursor = self.database.cursor().await?;
        let registered_probe = cursor.get_probe(session.probe_id).await?;
        cursor.rollback().await?;

        configuration
            .materialize_skills(&probe, &registered_probe.metadata, skills)
            .await
            .map_err(invalid)
    }

    /// Root agents have no parent, so `Parent` choices resolve to the session policy.
    async fn creation_model(
        &self,
        session: &sessions::Session,
        parent: Option<AgentId>,
        config: &AgentConfig,
        choice: Option<&ModelSelection>,
    ) -> Result<AgentModel, RuntimeError> {
        let parent_model = match parent {
            Some(id) => {
                let mut cursor = self.database.cursor().await?;
                let agent = cursor.require_agent_session(session.id, id).await?;
                let definition = cursor.get_agent_definition(agent.definition_id).await?;
                cursor.rollback().await?;
                definition.model
            }
            None => AgentModel::Global,
        };
        select_model(config, choice, &parent_model)
    }

    pub async fn create_configured_agent(
        &self,
        id: SessionId,
        request: api::CreateAgent,
    ) -> Result<Agent, RuntimeError> {
        let guard = self.configuration_gate.write().await;
        self.refresh_configuration(&[id]).await?;
        // Resolution only reads sources, so other creations may proceed alongside.
        let _guard = guard.downgrade();
        let session = self.get_session(id).await?;
        let mut cursor = self.database.cursor().await?;
        let session_group = cursor.get_session_agents_dir(id).await?;
        let graph = cursor
            .get_agent_config_graph(&request.config_id, session_group.as_ref())
            .await?;
        cursor.rollback().await?;
        let root = graph
            .configs
            .iter()
            .find(|config| config.config_id == request.config_id)
            .ok_or_else(|| invalid("agent configuration not found"))?;
        let model = self
            .creation_model(
                &session,
                request.subagent_parent_id,
                root,
                request.model.as_ref(),
            )
            .await?;
        let paths = self.materialize_skills(&session, &graph.skills).await?;
        let models = HashMap::from([(request.config_id.clone(), model)]);
        let mut cursor = self.database.cursor().await?;
        let snapshot = cursor
            .resolve_agent_config(&request.config_id, session_group.as_ref(), &paths, &models)
            .await?;
        cursor.commit().await?;
        self.commit_agent(
            id,
            crate::agents::NewAgent {
                definition_id: snapshot.definition.id,
                name: request.name,
                subagent_parent_id: request.subagent_parent_id,
                interactive: request.interactive,
                hidden: request.hidden,
            },
        )
        .await
    }

    /// Spawn a pinned permitted template without looking up its mutable source.
    pub async fn create_agent_with_model(
        &self,
        id: SessionId,
        mut request: crate::agents::NewAgent,
        model: Option<ModelSelection>,
    ) -> Result<Agent, RuntimeError> {
        let guard = self.configuration_gate.write().await;
        let snapshot = self
            .get_resolved_agent_config(request.definition_id)
            .await?;
        if let Some(snapshot) = snapshot {
            self.refresh_configuration(&[id]).await?;
            let _guard = guard.downgrade();
            let session = self.get_session(id).await?;
            let model = self
                .creation_model(
                    &session,
                    request.subagent_parent_id,
                    &snapshot.config,
                    model.as_ref(),
                )
                .await?;
            let mut cursor = self.database.cursor().await?;
            let current = cursor.list_skills().await?;
            cursor.rollback().await?;
            let skills: Vec<_> = current
                .into_iter()
                .filter(|skill| snapshot.references.skills.contains(&skill.id))
                .collect();
            let paths = self.materialize_skills(&session, &skills).await?;
            let mut cursor = self.database.cursor().await?;
            let resolved = cursor
                .resolve_pinned_agent_config(request.definition_id, model, &paths)
                .await?;
            cursor.commit().await?;
            request.definition_id = resolved.definition.id;
        } else if model.is_some() {
            return Err(invalid("model choices require a configured agent snapshot"));
        }
        self.commit_agent(id, request).await
    }

    #[cfg(test)]
    pub(crate) async fn create_agent(
        &self,
        id: SessionId,
        request: crate::agents::NewAgent,
    ) -> Result<Agent, RuntimeError> {
        self.create_agent_with_model(id, request, None).await
    }
}

/// Parent choices copy the parent's policy, so a child of a `Global` parent keeps
/// following the session model.
fn select_model(
    config: &AgentConfig,
    choice: Option<&ModelSelection>,
    parent: &AgentModel,
) -> Result<AgentModel, RuntimeError> {
    let selection = match choice {
        Some(choice) => config
            .models
            .iter()
            .find(|selection| *selection == choice)
            .ok_or_else(|| invalid("model choice is not permitted by this agent definition"))?,
        None if config.models.len() == 1 || config.first_model_default => config
            .models
            .first()
            .ok_or_else(|| invalid("models must not be empty"))?,
        None => {
            return Err(invalid(
                "an explicit model choice is required for this agent definition",
            ));
        }
    };
    match selection {
        ModelSelection::Global => Ok(AgentModel::Global),
        ModelSelection::Parent => Ok(parent.clone()),
        ModelSelection::Fixed { model, effort } => {
            let effort = match effort {
                EffortSelection::Fixed(effort) => AgentEffort::Fixed(*effort),
                EffortSelection::Parent => parent.effort(),
                EffortSelection::Global => AgentEffort::Global,
            };
            Ok(AgentModel::Fixed {
                model: model.clone(),
                effort,
            })
        }
    }
}
