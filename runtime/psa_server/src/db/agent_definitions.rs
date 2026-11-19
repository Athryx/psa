use super::{
    DatabaseCursor, DbError,
    schema::{decode_integer, encode_integer},
};
use psa::api::{AgentDefinition, AgentDefinitionId, AgentModel, BundleConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

struct DefinitionRow {
    id: i64,
    previous_version_id: Option<i64>,
    version: i64,
    name: String,
    description: String,
    selectable: bool,
    bundles: String,
    model_config: Option<String>,
    skills: String,
    permitted_subagents: String,
}

impl TryFrom<DefinitionRow> for AgentDefinition {
    type Error = DbError;

    fn try_from(row: DefinitionRow) -> Result<Self, DbError> {
        Ok(Self {
            id: AgentDefinitionId(decode_integer(row.id)?),
            previous_version_id: row
                .previous_version_id
                .map(decode_integer)
                .transpose()?
                .map(AgentDefinitionId),
            version: decode_integer(row.version)?,
            name: row.name,
            description: row.description,
            selectable: row.selectable,
            bundles: serde_json::from_str(&row.bundles)?,
            model: row
                .model_config
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default(),
            skills: serde_json::from_str(&row.skills)?,
            permitted_subagents: serde_json::from_str(&row.permitted_subagents)?,
        })
    }
}

impl DatabaseCursor {
    pub async fn create_agent_definition(
        &mut self,
        request: CreateAgentDefinition,
    ) -> Result<AgentDefinition, DbError> {
        self.insert_agent_definition(None, 1, request).await
    }

    pub async fn get_agent_definition(
        &mut self,
        id: AgentDefinitionId,
    ) -> Result<AgentDefinition, DbError> {
        let key = encode_integer(id.0)?;
        let row = sqlx::query_as!(DefinitionRow,
            "SELECT id, previous_version_id, version, name, description, selectable, model_config, skills, permitted_subagents, bundles FROM agent_definitions WHERE id = ?", key)
            .fetch_optional(self.connection()).await?
            .ok_or(DbError::NotFound { entity: "agent definition", id: id.0 })?;
        row.try_into()
    }

    /// Lists every immutable version, including versions superseded by an update.
    pub async fn list_agent_definitions(&mut self) -> Result<Vec<AgentDefinition>, DbError> {
        let rows = sqlx::query_as!(DefinitionRow,
            "SELECT id, previous_version_id, version, name, description, selectable, model_config, skills, permitted_subagents, bundles FROM agent_definitions ORDER BY id")
            .fetch_all(self.connection()).await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn update_agent_definition(
        &mut self,
        id: AgentDefinitionId,
        request: UpdateAgentDefinition,
    ) -> Result<AgentDefinition, DbError> {
        let previous = self.get_agent_definition(id).await?;
        let version = previous
            .version
            .checked_add(1)
            .ok_or_else(|| DbError::InvalidInput("agent definition version exhausted".into()))?;
        self.insert_agent_definition(Some(id), version, request)
            .await
    }

    async fn insert_agent_definition(
        &mut self,
        previous: Option<AgentDefinitionId>,
        version: u64,
        request: CreateAgentDefinition,
    ) -> Result<AgentDefinition, DbError> {
        crate::components::bundles::validate_completion_config(&request.bundles)
            .map_err(|error| DbError::InvalidInput(error.to_string()))?;

        let previous = previous.map(|id| encode_integer(id.0)).transpose()?;
        let version = encode_integer(version)?;
        let bundles = serde_json::to_string(&request.bundles)?;
        let model_config = serde_json::to_string(&request.model)?;
        let skills = serde_json::to_string(&request.skills)?;
        let permitted_subagents = serde_json::to_string(&request.permitted_subagents)?;
        let result = sqlx::query!(
            "INSERT INTO agent_definitions (previous_version_id, version, name, description, selectable, model_config, skills, permitted_subagents, bundles) VALUES (?,?,?,?,?,?,?,?,?)",
            previous, version, request.name, request.description, request.selectable, model_config, skills, permitted_subagents, bundles
        ).execute(self.connection()).await?;
        self.get_agent_definition(AgentDefinitionId(decode_integer(
            result.last_insert_rowid(),
        )?))
        .await
    }
}

/// Configuration for creating an immutable agent definition version.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentDefinition {
    /// Definition name.
    pub name: String,
    /// Human-readable description; defaults to empty.
    #[serde(default)]
    pub description: String,
    /// Whether this definition is offered for selection; defaults to false.
    #[serde(default)]
    pub selectable: bool,
    /// Ordered feature bundle configurations; defaults to empty.
    #[serde(default)]
    pub bundles: Vec<BundleConfig>,
    /// Model policy; defaults to following the session model.
    #[serde(default)]
    pub model: AgentModel,
    /// Paths to skill files or directories; defaults to empty.
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    /// Allowed subagent definition versions; defaults to empty.
    #[serde(default)]
    pub permitted_subagents: Vec<AgentDefinitionId>,
}

/// A complete replacement that creates a new immutable definition version.
pub type UpdateAgentDefinition = CreateAgentDefinition;
