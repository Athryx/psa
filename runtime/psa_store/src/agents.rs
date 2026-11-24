use super::{
    DbError, ReadCursor, WriteCursor,
    schema::{AgentRow, encode_integer, now_us, validate_name},
    sessions::get_session,
};
use crate::SessionId;
use llm_api::api::Usage;
use psa::api::events::EventId;
use psa::api::{AgentDefinitionId, AgentId, AgentState};
use sqlx::{Acquire, SqliteConnection};
use time::OffsetDateTime;

/// Represents the state of the agent in the database, mostly immutable except for state
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub definition_id: AgentDefinitionId,
    pub forked_from: Option<AgentId>,
    pub subagent_parent_id: Option<AgentId>,
    pub first_event_id: EventId,
    pub next_event_id: EventId,
    pub created_position: u64,
    pub name: String,
    pub interactive: bool,
    pub hidden: bool,
    pub state: AgentState,
    pub usage: Usage,
    pub cache: crate::CacheTracking,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl From<Agent> for psa::api::Agent {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            session_id: agent.session_id,
            definition_id: agent.definition_id,
            forked_from: agent.forked_from,
            subagent_parent_id: agent.subagent_parent_id,
            name: agent.name,
            interactive: agent.interactive,
            hidden: agent.hidden,
            state: agent.state,
            usage: agent.usage,
            cacheable_input_tokens: agent.cache.cacheable_input_tokens,
            next_event_id: agent.next_event_id,
            created_at: agent.created_at,
            updated_at: agent.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub subagent_parent_id: Option<AgentId>,
    pub definition_id: AgentDefinitionId,
    pub name: String,
    pub interactive: bool,
    pub hidden: bool,
}

impl ReadCursor {
    pub async fn get_agent(&mut self, id: AgentId) -> Result<Agent, DbError> {
        get_agent(self.connection(), id).await
    }

    pub async fn list_agents(&mut self, session: SessionId) -> Result<Vec<Agent>, DbError> {
        get_session(self.connection(), session).await?;
        list_agents(self.connection(), session).await
    }
}

impl WriteCursor {
    pub async fn create_agent(
        &mut self,
        session: SessionId,
        new: NewAgent,
    ) -> Result<Agent, DbError> {
        validate_name(&new.name)?;

        let mut savepoint = self.transaction.begin().await?;
        let owner = get_session(&mut savepoint, session).await?;
        if let Some(parent) = new.subagent_parent_id {
            let parent = get_agent(&mut savepoint, parent).await?;
            if parent.session_id != session {
                return Err(DbError::InvalidInput(
                    "subagent parent must belong to the same session".into(),
                ));
            }

            if list_agents(&mut savepoint, session)
                .await?
                .iter()
                .any(|agent| agent.name == new.name)
            {
                return Err(DbError::InvalidInput(format!(
                    "Agent name already exists in session: {}",
                    new.name
                )));
            }
        }

        let subagent_parent = new
            .subagent_parent_id
            .map(|id| encode_integer(id.0))
            .transpose()?;
        let session_id = encode_integer(session.0)?;
        let definition_id = encode_integer(new.definition_id.0)?;
        let position = encode_integer(owner.next_position)?;
        let next = position
            .checked_add(1)
            .ok_or_else(|| DbError::InvalidInput("session position exhausted".into()))?;
        let now = now_us()?;

        let result = sqlx::query!(
            "INSERT INTO agents (session_id, definition_id, subagent_parent_id, first_event_seq, next_event_seq, created_position, name, interactive, hidden, created_at_us, updated_at_us) VALUES (?,?,?,1,1,?,?,?,?,?,?)",
            session_id,
            definition_id,
            subagent_parent,
            position,
            new.name,
            new.interactive,
            new.hidden,
            now,
            now
        )
        .execute(&mut *savepoint)
        .await?;
        // The schema's legacy default predates cache-write tracking.
        sqlx::query("UPDATE agents SET usage = ? WHERE id = ?")
            .bind(super::cache_usage::encode_stored_usage(
                &llm_api::api::Usage::zero(),
                super::CacheTracking::start(),
            )?)
            .bind(result.last_insert_rowid())
            .execute(&mut *savepoint)
            .await?;
        sqlx::query!(
            "UPDATE sessions SET next_position = ?, updated_at_us = ? WHERE id = ?",
            next,
            now,
            session_id
        )
        .execute(&mut *savepoint)
        .await?;
        let agent = get_agent(
            &mut savepoint,
            AgentId(super::schema::decode_integer(result.last_insert_rowid())?),
        )
        .await?;

        savepoint.commit().await?;
        Ok(agent)
    }
}

pub(super) async fn get_agent(conn: &mut SqliteConnection, id: AgentId) -> Result<Agent, DbError> {
    let key = encode_integer(id.0)?;
    let row = sqlx::query_as!(
        AgentRow,
        "SELECT id, session_id, definition_id, parent_agent_id, subagent_parent_id, first_event_seq, next_event_seq, created_position, name, interactive, hidden, state, usage, created_at_us, updated_at_us FROM agents WHERE id = ?",
        key
    )
    .fetch_optional(conn)
    .await?
    .ok_or(DbError::NotFound { entity: "agent", id: id.0 })?;

    row.try_into()
}

pub(super) async fn list_agents(
    conn: &mut SqliteConnection,
    session: SessionId,
) -> Result<Vec<Agent>, DbError> {
    let key = encode_integer(session.0)?;
    let rows = sqlx::query_as!(
        AgentRow,
        "SELECT id, session_id, definition_id, parent_agent_id, subagent_parent_id, first_event_seq, next_event_seq, created_position, name, interactive, hidden, state, usage, created_at_us, updated_at_us FROM agents WHERE session_id = ? ORDER BY id",
        key
    )
    .fetch_all(conn)
    .await?;

    rows.into_iter().map(TryInto::try_into).collect()
}
