use super::{
    Database, DbError,
    agents::{get_agent, list_agents},
    events::{event_cutoff_at, load_event_rows},
    schema::{SessionRow, decode_integer, encode_integer, encode_path, now_us, validate_name},
};
use crate::{
    agents::{
        Agent, AgentId,
        events::{EventId, EventRange},
    },
    sessions::{
        ForkPoint, ForkedSession, NewSession, Session, SessionFork, SessionId, SessionUpdate,
    },
};
use sqlx::SqliteConnection;

impl Database {
    pub async fn create_session(&self, new: NewSession) -> Result<Session, DbError> {
        validate_name(&new.name)?;
        let path = encode_path(&new.working_directory)?;
        let now = now_us()?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query!(
            "INSERT INTO sessions (first_position, next_position, name, working_directory, created_at_us, updated_at_us) VALUES (1,1,?,?,?,?)",
            new.name,
            path,
            now,
            now
        )
        .execute(&mut *tx)
        .await?;
        let session = get_session(
            &mut tx,
            SessionId(decode_integer(result.last_insert_rowid())?),
        )
        .await?;

        tx.commit().await?;
        Ok(session)
    }

    pub async fn get_session(&self, id: SessionId) -> Result<Session, DbError> {
        get_session(&mut *self.pool.acquire().await?, id).await
    }

    /// `None` lists roots, `Some(id)` lists immediate children.
    pub async fn list_sessions(&self, parent: Option<SessionId>) -> Result<Vec<Session>, DbError> {
        let parent = parent.map(|id| encode_integer(id.0)).transpose()?;
        let rows = sqlx::query_as!(
            SessionRow,
            "SELECT id, parent_session_id, first_position, next_position, name, working_directory, created_at_us, updated_at_us FROM sessions WHERE parent_session_id IS ? ORDER BY id",
            parent
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn update_session(
        &self,
        id: SessionId,
        update: SessionUpdate,
    ) -> Result<Session, DbError> {
        if let Some(name) = &update.name {
            validate_name(name)?;
        }
        let path = update
            .working_directory
            .as_deref()
            .map(encode_path)
            .transpose()?;
        let key = encode_integer(id.0)?;
        let now = now_us()?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        get_session(&mut tx, id).await?;
        sqlx::query!(
            "UPDATE sessions SET name = COALESCE(?, name), working_directory = COALESCE(?, working_directory), updated_at_us = ? WHERE id = ?",
            update.name,
            path,
            now,
            key
        )
        .execute(&mut *tx)
        .await?;
        let session = get_session(&mut tx, id).await?;

        tx.commit().await?;
        Ok(session)
    }

    pub async fn fork_session(
        &self,
        source: SessionId,
        fork: SessionFork,
    ) -> Result<ForkedSession, DbError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let original = get_session(&mut tx, source).await?;

        let position = match fork.point {
            ForkPoint::Head => original.next_position - 1,
            ForkPoint::ThroughEvent { agent_id, event_id } => {
                let agent = get_agent(&mut tx, agent_id).await?;
                if agent.session_id != source || event_id.0 == 0 {
                    return Err(DbError::InvalidInput(
                        "fork event must belong to a stream in the source session".into(),
                    ));
                }

                let range = EventRange {
                    after: EventId(event_id.0 - 1),
                    through: Some(event_id),
                    limit: 1,
                };
                let rows = load_event_rows(&mut tx, agent_id, &range).await?;
                decode_integer(
                    rows.first()
                        .ok_or_else(|| DbError::InvalidInput("fork event is not visible".into()))?
                        .session_position,
                )?
            }
        };

        let name = fork.metadata.name.unwrap_or(original.name);
        let path = fork
            .metadata
            .working_directory
            .unwrap_or(original.working_directory);
        validate_name(&name)?;
        let path = encode_path(&path)?;
        let parent = encode_integer(source.0)?;
        let first = encode_integer(position)?
            .checked_add(1)
            .ok_or_else(|| DbError::InvalidInput("session position exhausted".into()))?;
        let now = now_us()?;

        let result = sqlx::query!(
            "INSERT INTO sessions (parent_session_id, first_position, next_position, name, working_directory, created_at_us, updated_at_us) VALUES (?,?,?,?,?,?,?)",
            parent,
            first,
            first,
            name,
            path,
            now,
            now
        )
        .execute(&mut *tx)
        .await?;
        let child_id = SessionId(decode_integer(result.last_insert_rowid())?);

        let mut agents = Vec::new();
        for agent in list_agents(&mut tx, source).await? {
            if agent.created_position <= position {
                let child = fork_agent(&mut tx, &agent, child_id, position).await?;
                agents.push((agent.id, child));
            }
        }
        let session = get_session(&mut tx, child_id).await?;

        tx.commit().await?;
        Ok(ForkedSession { session, agents })
    }
}

pub(super) async fn get_session(
    conn: &mut SqliteConnection,
    id: SessionId,
) -> Result<Session, DbError> {
    let key = encode_integer(id.0)?;
    let row = sqlx::query_as!(
        SessionRow,
        "SELECT id, parent_session_id, first_position, next_position, name, working_directory, created_at_us, updated_at_us FROM sessions WHERE id = ?",
        key
    )
    .fetch_optional(conn)
    .await?
    .ok_or(DbError::NotFound { entity: "session", id: id.0 })?;

    row.try_into()
}

async fn fork_agent(
    conn: &mut SqliteConnection,
    agent: &Agent,
    session: SessionId,
    position: u64,
) -> Result<Agent, DbError> {
    let cutoff = event_cutoff_at(conn, agent.id, position).await?;
    let first = cutoff
        .checked_add(1)
        .ok_or_else(|| DbError::InvalidInput("event sequence exhausted".into()))?;
    let session_id = encode_integer(session.0)?;
    let parent = encode_integer(agent.id.0)?;
    let created = encode_integer(agent.created_position)?;
    let now = now_us()?;

    let result = sqlx::query!(
        "INSERT INTO agents (session_id, parent_agent_id, first_event_seq, next_event_seq, created_position, name, created_at_us, updated_at_us) VALUES (?,?,?,?,?,?,?,?)",
        session_id,
        parent,
        first,
        first,
        created,
        agent.name,
        now,
        now
    )
    .execute(&mut *conn)
    .await?;

    get_agent(conn, AgentId(decode_integer(result.last_insert_rowid())?)).await
}
