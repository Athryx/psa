use super::{
    DatabaseCursor, DbError,
    agents::{get_agent, list_agents},
    events::{agent_state_at, event_cutoff_at, load_event_rows},
    schema::{
        SessionRow, decode_integer, encode_agent_state, encode_integer, encode_path, now_us,
        validate_name,
    },
};
use crate::db::EventRange;
use crate::{
    agents::{Agent, AgentId},
    sessions::{
        ForkPoint, ForkedSession, NewSession, Session, SessionFork, SessionId, SessionUpdate,
    },
};
use psa_api::events::EventId;
use sqlx::{Acquire, SqliteConnection};

impl DatabaseCursor {
    pub async fn create_session(&mut self, new: NewSession) -> Result<Session, DbError> {
        self.get_probe(new.probe_id).await?;
        let probe_id = new.probe_id.to_string();
        let model_config = serde_json::to_string(&new.model_config)?;
        let decision_model = new
            .decision_model
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        validate_name(&new.name)?;
        if let Some(project) = &new.project_id {
            project.validate().map_err(DbError::InvalidInput)?;
        }
        let project_id = new.project_id.map(|project| project.0);
        let path = encode_path(&new.working_directory)?;
        let now = now_us()?;

        let mut savepoint = self.transaction.begin().await?;
        let result = sqlx::query!(
            "INSERT INTO sessions (first_position, next_position, name, working_directory, probe_id, project_id, model_config, decision_model, created_at_us, updated_at_us) VALUES (1,1,?,?,?,?,?,?,?,?)",
            new.name,
            path,
            probe_id,
            project_id,
            model_config,
            decision_model,
            now,
            now
        )
        .execute(&mut *savepoint)
        .await?;
        let session = get_session(
            &mut savepoint,
            SessionId(decode_integer(result.last_insert_rowid())?),
        )
        .await?;

        savepoint.commit().await?;
        Ok(session)
    }

    pub async fn get_session(&mut self, id: SessionId) -> Result<Session, DbError> {
        get_session(self.connection(), id).await
    }

    /// `None` lists roots, `Some(id)` lists immediate children.
    pub async fn list_sessions(
        &mut self,
        parent: Option<SessionId>,
    ) -> Result<Vec<Session>, DbError> {
        let parent = parent.map(|id| encode_integer(id.0)).transpose()?;
        let rows = sqlx::query_as!(
            SessionRow,
            "SELECT id, parent_session_id, first_position, next_position, name, working_directory, probe_id, project_id, model_config, decision_model, created_at_us, updated_at_us FROM sessions WHERE parent_session_id IS ? ORDER BY id",
            parent
        )
        .fetch_all(self.connection())
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Summary keyset pages; runtime SQL avoids adding prepared-cache entries.
    /// Filter semantics must stay identical to [`psa_api::SessionFilter::matches`]:
    /// SQLite's `lower` folds only ASCII, like the Rust comparison.
    pub async fn query_sessions(
        &mut self,
        query: &psa_api::SessionsQuery,
        before: Option<(i64, i64)>,
    ) -> Result<Vec<Session>, DbError> {
        use sqlx::Row;
        let parent = query.parent.map(|id| encode_integer(id.0)).transpose()?;
        let filter = &query.filter;
        let probes = serde_json::to_string(&filter.probe_ids)?;
        let includes_global = filter.project_ids.contains(&None);
        let projects =
            serde_json::to_string(&filter.project_ids.iter().flatten().collect::<Vec<_>>())?;
        let rows = sqlx::query(
            "SELECT id, parent_session_id, first_position, next_position, name, working_directory, probe_id, project_id, model_config, decision_model, created_at_us, updated_at_us FROM sessions \
             WHERE (? OR parent_session_id IS ?) \
             AND instr(lower(name), lower(?)) > 0 \
             AND (? OR probe_id IN (SELECT value FROM json_each(?))) \
             AND (? OR (project_id IS NULL AND ?) OR project_id IN (SELECT value FROM json_each(?))) \
             AND (? IS NULL OR updated_at_us < ? OR (updated_at_us = ? AND id < ?)) \
             ORDER BY updated_at_us DESC, id DESC LIMIT ?",
        )
        .bind(query.flat).bind(parent)
        .bind(&filter.name_contains)
        .bind(filter.probe_ids.is_empty()).bind(&probes)
        .bind(filter.project_ids.is_empty()).bind(includes_global).bind(&projects)
        .bind(before.map(|v| v.0)).bind(before.map(|v| v.0))
        .bind(before.map(|v| v.0)).bind(before.map(|v| v.1))
        .bind(i64::from(query.limit) + 1)
        .fetch_all(self.connection()).await?;
        rows.into_iter()
            .map(|row| {
                SessionRow {
                    id: row.try_get("id")?,
                    parent_session_id: row.try_get("parent_session_id")?,
                    first_position: row.try_get("first_position")?,
                    next_position: row.try_get("next_position")?,
                    name: row.try_get("name")?,
                    working_directory: row.try_get("working_directory")?,
                    probe_id: row.try_get("probe_id")?,
                    project_id: row.try_get("project_id")?,
                    model_config: row.try_get("model_config")?,
                    decision_model: row.try_get("decision_model")?,
                    created_at_us: row.try_get("created_at_us")?,
                    updated_at_us: row.try_get("updated_at_us")?,
                }
                .try_into()
            })
            .collect()
    }

    /// Distinct projects of existing sessions, sorted.
    pub async fn list_projects(&mut self) -> Result<Vec<psa_api::ProjectId>, DbError> {
        let rows = sqlx::query_scalar!(
            "SELECT DISTINCT project_id AS \"project_id!\" FROM sessions WHERE project_id IS NOT NULL ORDER BY project_id"
        )
        .fetch_all(self.connection())
        .await?;
        Ok(rows.into_iter().map(psa_api::ProjectId).collect())
    }

    pub async fn update_session(
        &mut self,
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
        let model_config = update
            .model_config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let decision_model = update
            .decision_model
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let key = encode_integer(id.0)?;
        let now = now_us()?;

        let mut savepoint = self.transaction.begin().await?;
        get_session(&mut savepoint, id).await?;
        sqlx::query!(
            "UPDATE sessions SET name = COALESCE(?, name), working_directory = COALESCE(?, working_directory), model_config = COALESCE(?, model_config), decision_model = COALESCE(?, decision_model), updated_at_us = ? WHERE id = ?",
            update.name,
            path,
            model_config,
            decision_model,
            now,
            key
        )
        .execute(&mut *savepoint)
        .await?;
        let session = get_session(&mut savepoint, id).await?;

        savepoint.commit().await?;
        Ok(session)
    }

    pub async fn fork_session(
        &mut self,
        source: SessionId,
        fork: SessionFork,
    ) -> Result<ForkedSession, DbError> {
        let mut savepoint = self.transaction.begin().await?;
        let original = get_session(&mut savepoint, source).await?;

        let position = match fork.point {
            ForkPoint::Head => original.next_position - 1,
            ForkPoint::ThroughEvent { agent_id, event_id } => {
                let agent = get_agent(&mut savepoint, agent_id).await?;
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
                let rows = load_event_rows(&mut savepoint, agent_id, &range).await?;
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
        let probe_id = original.probe_id.to_string();
        let project_id = original.project_id.map(|project| project.0);
        let model_config =
            serde_json::to_string(&fork.metadata.model_config.unwrap_or(original.model_config))?;
        let decision_model = fork
            .metadata
            .decision_model
            .or(original.decision_model)
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let first = encode_integer(position)?
            .checked_add(1)
            .ok_or_else(|| DbError::InvalidInput("session position exhausted".into()))?;
        let now = now_us()?;

        let result = sqlx::query!(
            "INSERT INTO sessions (parent_session_id, first_position, next_position, name, working_directory, probe_id, project_id, model_config, decision_model, created_at_us, updated_at_us) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            parent,
            first,
            first,
            name,
            path,
            probe_id,
            project_id,
            model_config,
            decision_model,
            now,
            now
        )
        .execute(&mut *savepoint)
        .await?;
        let child_id = SessionId(decode_integer(result.last_insert_rowid())?);

        let originals = list_agents(&mut savepoint, source).await?;
        let mut agents = Vec::new();
        let mut copied_agent_ids = std::collections::HashMap::new();
        for agent in &originals {
            if agent.created_position <= position {
                let child = fork_agent(&mut savepoint, agent, child_id, position).await?;
                copied_agent_ids.insert(agent.id, child.id);
                agents.push(child);
            }
        }

        for copy in &mut agents {
            let original = originals
                .iter()
                .find(|agent| Some(agent.id) == copy.parent_id)
                .expect("fork source exists");
            copy.subagent_parent_id = original
                .subagent_parent_id
                .and_then(|parent| copied_agent_ids.get(&parent).copied());
            let parent = copy
                .subagent_parent_id
                .map(|id| encode_integer(id.0))
                .transpose()?;
            let id = encode_integer(copy.id.0)?;
            sqlx::query!(
                "UPDATE agents SET subagent_parent_id = ? WHERE id = ?",
                parent,
                id
            )
            .execute(&mut *savepoint)
            .await?;
        }

        let session = get_session(&mut savepoint, child_id).await?;

        savepoint.commit().await?;
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
        "SELECT id, parent_session_id, first_position, next_position, name, working_directory, probe_id, project_id, model_config, decision_model, created_at_us, updated_at_us FROM sessions WHERE id = ?",
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
    let state =
        encode_agent_state(agent_state_at(conn, agent.id, EventId(decode_integer(cutoff)?)).await?);
    let usage = serde_json::to_string(
        &super::queries::usage_at(conn, agent.id, EventId(decode_integer(cutoff)?)).await?,
    )?;
    let first = cutoff
        .checked_add(1)
        .ok_or_else(|| DbError::InvalidInput("event sequence exhausted".into()))?;
    let session_id = encode_integer(session.0)?;
    let parent = encode_integer(agent.id.0)?;
    let definition_id = encode_integer(agent.definition_id.0)?;
    let created = encode_integer(agent.created_position)?;
    let now = now_us()?;

    let result = sqlx::query!(
        "INSERT INTO agents (session_id, definition_id, parent_agent_id, first_event_seq, next_event_seq, created_position, name, interactive, hidden, state, usage, created_at_us, updated_at_us) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        session_id,
        definition_id,
        parent,
        first,
        first,
        created,
        agent.name,
        agent.interactive,
        agent.hidden,
        state,
        usage,
        now,
        now
    )
    .execute(&mut *conn)
    .await?;

    get_agent(conn, AgentId(decode_integer(result.last_insert_rowid())?)).await
}
