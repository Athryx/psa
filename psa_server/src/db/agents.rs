use super::{
    Database, DbError,
    schema::{AgentRow, encode_integer, now_us, validate_name},
    sessions::get_session,
};
use crate::{
    agents::{Agent, AgentId, NewAgent},
    sessions::SessionId,
};
use sqlx::SqliteConnection;

impl Database {
    pub async fn create_agent(&self, session: SessionId, new: NewAgent) -> Result<Agent, DbError> {
        validate_name(&new.name)?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owner = get_session(&mut tx, session).await?;
        let session_id = encode_integer(session.0)?;
        let position = encode_integer(owner.next_position)?;
        let next = position
            .checked_add(1)
            .ok_or_else(|| DbError::InvalidInput("session position exhausted".into()))?;
        let now = now_us()?;

        let result = sqlx::query!(
            "INSERT INTO agents (session_id, first_event_seq, next_event_seq, created_position, name, created_at_us, updated_at_us) VALUES (?,1,1,?,?,?,?)",
            session_id,
            position,
            new.name,
            now,
            now
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE sessions SET next_position = ?, updated_at_us = ? WHERE id = ?",
            next,
            now,
            session_id
        )
        .execute(&mut *tx)
        .await?;
        let agent = get_agent(
            &mut tx,
            AgentId(super::schema::decode_integer(result.last_insert_rowid())?),
        )
        .await?;

        tx.commit().await?;
        Ok(agent)
    }

    pub async fn get_agent(&self, id: AgentId) -> Result<Agent, DbError> {
        get_agent(&mut *self.pool.acquire().await?, id).await
    }

    pub async fn list_agents(&self, session: SessionId) -> Result<Vec<Agent>, DbError> {
        let mut tx = self.pool.begin().await?;
        get_session(&mut tx, session).await?;
        let result = list_agents(&mut tx, session).await?;

        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn get_agent(conn: &mut SqliteConnection, id: AgentId) -> Result<Agent, DbError> {
    let key = encode_integer(id.0)?;
    let row = sqlx::query_as!(
        AgentRow,
        "SELECT id, session_id, parent_agent_id, first_event_seq, next_event_seq, created_position, name, created_at_us, updated_at_us FROM agents WHERE id = ?",
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
        "SELECT id, session_id, parent_agent_id, first_event_seq, next_event_seq, created_position, name, created_at_us, updated_at_us FROM agents WHERE session_id = ? ORDER BY id",
        key
    )
    .fetch_all(conn)
    .await?;

    rows.into_iter().map(TryInto::try_into).collect()
}
