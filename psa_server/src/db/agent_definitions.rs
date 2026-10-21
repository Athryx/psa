use super::{
    DatabaseCursor, DbError,
    schema::{decode_integer, encode_integer},
};
use psa_api::{
    AgentDefinition, AgentDefinitionId, CreateAgentDefinition, HookIds, UpdateAgentDefinition,
};

struct DefinitionRow {
    id: i64,
    previous_version_id: Option<i64>,
    version: i64,
    name: String,
    description: String,
    selectable: bool,
    tools: String,
    agent_create_hooks: String,
    session_update_hooks: String,
    before_request_hooks: String,
    after_request_hooks: String,
    model_config: Option<String>,
    skills: String,
    permitted_subagents: String,
    extra_system_prompt: String,
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
            tools: serde_json::from_str(&row.tools)?,
            hooks: HookIds {
                agent_create: serde_json::from_str(&row.agent_create_hooks)?,
                session_update: serde_json::from_str(&row.session_update_hooks)?,
                before_request: serde_json::from_str(&row.before_request_hooks)?,
                after_request: serde_json::from_str(&row.after_request_hooks)?,
            },
            model_config: row
                .model_config
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            skills: serde_json::from_str(&row.skills)?,
            permitted_subagents: serde_json::from_str(&row.permitted_subagents)?,
            extra_system_prompt: row.extra_system_prompt,
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
            "SELECT id, previous_version_id, version, name, description, selectable, tools, agent_create_hooks, session_update_hooks, before_request_hooks, after_request_hooks, model_config, skills, permitted_subagents, extra_system_prompt FROM agent_definitions WHERE id = ?", key)
            .fetch_optional(self.connection()).await?
            .ok_or(DbError::NotFound { entity: "agent definition", id: id.0 })?;
        row.try_into()
    }

    /// Lists every immutable version, including versions superseded by an update.
    pub async fn list_agent_definitions(&mut self) -> Result<Vec<AgentDefinition>, DbError> {
        let rows = sqlx::query_as!(DefinitionRow,
            "SELECT id, previous_version_id, version, name, description, selectable, tools, agent_create_hooks, session_update_hooks, before_request_hooks, after_request_hooks, model_config, skills, permitted_subagents, extra_system_prompt FROM agent_definitions ORDER BY id")
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
        let previous = previous.map(|id| encode_integer(id.0)).transpose()?;
        let version = encode_integer(version)?;
        let tools = serde_json::to_string(&request.tools)?;
        let agent_create = serde_json::to_string(&request.hooks.agent_create)?;
        let session_update = serde_json::to_string(&request.hooks.session_update)?;
        let before_request = serde_json::to_string(&request.hooks.before_request)?;
        let after_request = serde_json::to_string(&request.hooks.after_request)?;
        let model_config = request
            .model_config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let skills = serde_json::to_string(&request.skills)?;
        let permitted_subagents = serde_json::to_string(&request.permitted_subagents)?;
        let result = sqlx::query!(
            "INSERT INTO agent_definitions (previous_version_id, version, name, description, selectable, tools, agent_create_hooks, session_update_hooks, before_request_hooks, after_request_hooks, model_config, skills, permitted_subagents, extra_system_prompt) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            previous, version, request.name, request.description, request.selectable, tools, agent_create, session_update, before_request, after_request, model_config, skills, permitted_subagents, request.extra_system_prompt
        ).execute(self.connection()).await?;
        self.get_agent_definition(AgentDefinitionId(decode_integer(
            result.last_insert_rowid(),
        )?))
        .await
    }
}
