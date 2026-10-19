use super::{
    Database, DbError,
    agents::get_agent,
    payloads::{event_kind, insert_payload, load_payloads},
    schema::{EventRow, decode_integer, decode_time, encode_integer, encode_time, now_us},
    sessions::get_session,
};
use crate::agents::{
    AgentId,
    events::{Event, EventId, EventRange, EventType, NewEvent},
};
use sqlx::SqliteConnection;
use std::collections::HashSet;

impl Database {
    pub async fn add_events(
        &self,
        agent: AgentId,
        events: &[NewEvent],
    ) -> Result<Vec<Event>, DbError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let stream = get_agent(&mut tx, agent).await?;
        if events.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let session = get_session(&mut tx, stream.session_id).await?;
        let count = i64::try_from(events.len())
            .map_err(|_| DbError::InvalidInput("too many events".into()))?;
        let first_seq = encode_integer(stream.next_event_id.0)?;
        let first_position = encode_integer(session.next_position)?;
        let next_seq = first_seq
            .checked_add(count)
            .ok_or_else(|| DbError::InvalidInput("event sequence exhausted".into()))?;
        let next_position = first_position
            .checked_add(count)
            .ok_or_else(|| DbError::InvalidInput("session position exhausted".into()))?;
        let agent_id = encode_integer(agent.0)?;
        let session_id = encode_integer(session.id.0)?;
        let mut saved = Vec::with_capacity(events.len());

        for (offset, event) in events.iter().enumerate() {
            let seq = first_seq + offset as i64;
            let position = first_position + offset as i64;
            let timestamp = encode_time(event.timestamp)?;
            let mut payload = event.kind.clone();
            if let EventType::InferenceResultEnd(end) = &mut payload {
                let micros = u64::try_from(end.request_time.as_micros()).map_err(|_| {
                    DbError::InvalidInput("duration exceeds supported range".into())
                })?;
                end.request_time = std::time::Duration::from_micros(micros);
            }
            let kind = event_kind(&payload);

            let result = sqlx::query!(
                "INSERT INTO events (agent_id, event_seq, session_position, timestamp_us, kind) VALUES (?,?,?,?,?)",
                agent_id,
                seq,
                position,
                timestamp,
                kind
            )
            .execute(&mut *tx)
            .await?;
            insert_payload(&mut tx, result.last_insert_rowid(), &payload).await?;
            saved.push(Event {
                id: EventId(decode_integer(seq)?),
                source_agent_id: agent,
                timestamp: decode_time(timestamp)?,
                kind: payload,
            });
        }

        let now = now_us()?;
        sqlx::query!(
            "UPDATE agents SET next_event_seq = ?, updated_at_us = ? WHERE id = ?",
            next_seq,
            now,
            agent_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE sessions SET next_position = ?, updated_at_us = ? WHERE id = ?",
            next_position,
            now,
            session_id
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(saved)
    }

    pub async fn load_events(
        &self,
        agent: AgentId,
        range: EventRange,
    ) -> Result<Vec<Event>, DbError> {
        if range.limit == 0 || range.limit > 10_000 {
            return Err(DbError::InvalidInput(
                "event page size must be between 1 and 10000".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        let rows = load_event_rows(&mut tx, agent, &range).await?;
        let mut result = Vec::with_capacity(rows.len());

        // A page can span multiple ancestors. Load each owner's payloads in a batch.
        let mut owners = Vec::new();
        for row in &rows {
            if !owners.contains(&row.agent_id) {
                owners.push(row.agent_id);
            }
        }
        let mut payloads = std::collections::HashMap::new();
        for owner in owners {
            let first = rows.iter().find(|r| r.agent_id == owner).unwrap().event_seq;
            let last = rows
                .iter()
                .rev()
                .find(|r| r.agent_id == owner)
                .unwrap()
                .event_seq;
            payloads.extend(load_payloads(&mut tx, owner, first, last).await?);
        }

        for row in rows {
            let kind = payloads.remove(&row.event_pk).ok_or_else(|| {
                DbError::Corrupt(format!("event {} has no payload", row.event_pk))
            })?;
            if event_kind(&kind) != row.kind {
                return Err(DbError::Corrupt(
                    "payload kind differs from envelope".into(),
                ));
            }
            result.push(Event {
                id: EventId(decode_integer(row.event_seq)?),
                source_agent_id: AgentId(decode_integer(row.agent_id)?),
                timestamp: decode_time(row.timestamp_us)?,
                kind,
            });
        }

        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn load_event_rows(
    conn: &mut SqliteConnection,
    agent: AgentId,
    range: &EventRange,
) -> Result<Vec<EventRow>, DbError> {
    let segments = load_segments(conn, agent, range.after, range.through).await?;
    let mut rows = Vec::new();
    for (owner, lower, upper) in segments {
        let remaining = i64::from(range.limit) - rows.len() as i64;
        if remaining <= 0 {
            break;
        }
        let batch = sqlx::query_as!(
            EventRow,
            "SELECT event_pk, agent_id, event_seq, session_position, timestamp_us, kind FROM events WHERE agent_id = ? AND event_seq BETWEEN ? AND ? ORDER BY event_seq LIMIT ?",
            owner,
            lower,
            upper,
            remaining
        )
        .fetch_all(&mut *conn)
        .await?;
        let expected = remaining.min(upper - lower + 1);
        if batch.len() as i64 != expected
            || batch
                .iter()
                .enumerate()
                .any(|(i, r)| r.event_seq != lower + i as i64)
        {
            return Err(DbError::Corrupt("gap in event sequence".into()));
        }
        rows.extend(batch);
    }
    Ok(rows)
}

pub(super) async fn event_cutoff_at(
    conn: &mut SqliteConnection,
    agent: AgentId,
    position: u64,
) -> Result<i64, DbError> {
    let position = encode_integer(position)?;
    let segments = load_segments(conn, agent, EventId(0), None).await?;
    for (owner, lower, upper) in segments.into_iter().rev() {
        let row = sqlx::query!(
            "SELECT MAX(event_seq) AS event_seq FROM events WHERE agent_id = ? AND event_seq BETWEEN ? AND ? AND session_position <= ?",
            owner,
            lower,
            upper,
            position
        )
        .fetch_one(&mut *conn)
        .await?;
        if let Some(seq) = row.event_seq {
            return Ok(seq);
        }
    }
    Ok(0)
}

async fn load_segments(
    conn: &mut SqliteConnection,
    agent: AgentId,
    after: EventId,
    through: Option<EventId>,
) -> Result<Vec<(i64, i64, i64)>, DbError> {
    let mut current = get_agent(conn, agent).await?;
    let mut upper = encode_integer(through.map_or(current.next_event_id.0 - 1, |n| {
        n.0.min(current.next_event_id.0 - 1)
    }))?;
    let after = encode_integer(after.0)?;
    let mut segments = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.id) {
            return Err(DbError::Corrupt("cycle in agent ancestry".into()));
        }
        let first = encode_integer(current.first_event_id.0)?;
        if upper >= first && upper > after {
            segments.push((
                encode_integer(current.id.0)?,
                first.max(after.saturating_add(1)),
                upper,
            ));
        }
        upper = upper.min(first - 1);
        let Some(parent_id) = current.parent_id else {
            break;
        };
        let parent = match get_agent(conn, parent_id).await {
            Err(DbError::NotFound { .. }) => {
                return Err(DbError::Corrupt("missing agent parent".into()));
            }
            result => result?,
        };
        let owner = get_session(conn, current.session_id).await?;
        if owner.parent_id != Some(parent.session_id)
            || current.first_event_id.0 > parent.next_event_id.0
        {
            return Err(DbError::Corrupt(
                "agent ancestry disagrees with session ancestry or fork boundary".into(),
            ));
        }
        current = parent;
    }
    segments.reverse();
    Ok(segments)
}
