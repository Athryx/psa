use super::{
    DbError, ReadCursor, WriteCursor,
    agents::get_agent,
    payloads::{event_kind, insert_payload, load_payloads},
    schema::{
        EventRow, decode_integer, decode_time, encode_agent_state, encode_integer, encode_time,
        now_us, parse_agent_state,
    },
    sessions::get_session,
    snapshots::{decode_snapshot, encode_snapshot},
};
use crate::agents::{AgentId, AgentState};
use llm_api::api::Usage;
use psa::api::DecisionCall;
use psa::api::events::{Event, EventId, EventType};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, SqliteConnection};
use std::collections::HashSet;
use time::OffsetDateTime;

/// Maximum private codemode snapshot size accepted for one tool result.
pub const MAX_CODEMODE_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum saved full codemode output accepted for one tool result.
pub const MAX_CODEMODE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// An internal event batch item. Private events attach to the preceding public
/// event in this batch and never consume a public event ID.
#[derive(Debug, Clone)]
pub enum EventBatchItem {
    Public(NewEvent),
    Private(PrivateEvent),
}

#[derive(Clone, PartialEq, Eq)]
pub enum PrivateEvent {
    CodeModeSnapshot(Vec<u8>),
    /// Full codemode output when the model-facing result was truncated.
    CodeModeOutput(String),
}

impl std::fmt::Debug for PrivateEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CodeModeSnapshot(bytes) => f
                .debug_struct("CodeModeSnapshot")
                .field("len", &bytes.len())
                .finish(),
            Self::CodeModeOutput(text) => f
                .debug_struct("CodeModeOutput")
                .field("len", &text.len())
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateEventKind {
    CodeModeSnapshot,
    CodeModeOutput,
}

impl PrivateEvent {
    fn kind(&self) -> PrivateEventKind {
        match self {
            Self::CodeModeSnapshot(_) => PrivateEventKind::CodeModeSnapshot,
            Self::CodeModeOutput(_) => PrivateEventKind::CodeModeOutput,
        }
    }
}

/// The newest private event of `kind` on `owner`'s events in `lower..=upper`.
async fn query_private_event(
    conn: &mut SqliteConnection,
    kind: PrivateEventKind,
    owner: i64,
    lower: i64,
    upper: i64,
) -> Result<Option<PrivateEvent>, DbError> {
    let event = match kind {
        PrivateEventKind::CodeModeSnapshot => {
            let stored = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT s.snapshot
                 FROM codemode_snapshots s
                 JOIN events e ON e.event_pk = s.event_pk
                 WHERE e.agent_id = ? AND e.event_seq BETWEEN ? AND ?
                 ORDER BY e.event_seq DESC LIMIT 1",
            )
            .bind(owner)
            .bind(lower)
            .bind(upper)
            .fetch_optional(&mut *conn)
            .await?;
            let Some(stored) = stored else {
                return Ok(None);
            };
            let snapshot = decode_snapshot(stored).await?;
            Some(PrivateEvent::CodeModeSnapshot(snapshot))
        }
        PrivateEventKind::CodeModeOutput => sqlx::query_scalar::<_, String>(
            "SELECT o.output
             FROM codemode_outputs o
             JOIN events e ON e.event_pk = o.event_pk
             WHERE e.agent_id = ? AND e.event_seq BETWEEN ? AND ?
             ORDER BY e.event_seq DESC LIMIT 1",
        )
        .bind(owner)
        .bind(lower)
        .bind(upper)
        .fetch_optional(&mut *conn)
        .await?
        .map(PrivateEvent::CodeModeOutput),
    };
    Ok(event)
}

/// Changes staged by an append; commit the cursor to persist them.
#[derive(Debug, Clone)]
pub struct AddEventResult {
    pub events: Vec<Event>,
    /// The last state in the batch, only if it differs from the previous state.
    pub new_state: Option<AgentState>,
    pub new_usage: Option<Usage>,
}

impl ReadCursor {
    /// Load the latest private event of this kind visible through the agent's
    /// fork lineage, optionally bounded by an inclusive public event sequence.
    pub async fn load_latest_private_event(
        &mut self,
        agent: AgentId,
        through: Option<EventId>,
        kind: PrivateEventKind,
    ) -> Result<Option<PrivateEvent>, DbError> {
        let conn = self.connection();
        let segments = load_segments(conn, agent, EventId(0), through).await?;
        for (owner, lower, upper) in segments.into_iter().rev() {
            let event = query_private_event(conn, kind, owner, lower, upper).await?;
            if event.is_some() {
                return Ok(event);
            }
        }
        Ok(None)
    }

    /// Load the private event of this kind attached to exactly `event`,
    /// resolving the owning agent through the fork lineage.
    pub async fn load_private_event_at(
        &mut self,
        agent: AgentId,
        event: EventId,
        kind: PrivateEventKind,
    ) -> Result<Option<PrivateEvent>, DbError> {
        let conn = self.connection();
        let after = EventId(event.0.saturating_sub(1));
        let segments = load_segments(conn, agent, after, Some(event)).await?;
        let Some((owner, lower, upper)) = segments.into_iter().next() else {
            return Ok(None);
        };
        query_private_event(conn, kind, owner, lower, upper).await
    }

    pub async fn load_events(
        &mut self,
        agent: AgentId,
        range: EventRange,
    ) -> Result<Vec<Event>, DbError> {
        if range.limit == 0 || range.limit > 10_000 {
            return Err(DbError::InvalidInput(
                "event page size must be between 1 and 10000".into(),
            ));
        }

        let conn = self.connection();
        let rows = load_event_rows(conn, agent, &range).await?;
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
            payloads.extend(load_payloads(conn, owner, first, last).await?);
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

        Ok(result)
    }
}

impl WriteCursor {
    pub async fn add_events(
        &mut self,
        agent: AgentId,
        events: &[NewEvent],
    ) -> Result<AddEventResult, DbError> {
        let batch: Vec<_> = events.iter().cloned().map(EventBatchItem::Public).collect();
        self.add_event_batch(agent, &batch).await
    }

    /// Atomically stage public events and private attachments in batch order.
    pub async fn add_event_batch(
        &mut self,
        agent: AgentId,
        batch: &[EventBatchItem],
    ) -> Result<AddEventResult, DbError> {
        let mut events = Vec::new();
        let mut private_events: Vec<Vec<&PrivateEvent>> = Vec::new();
        for item in batch {
            match item {
                EventBatchItem::Public(event) => {
                    events.push(event);
                    private_events.push(Vec::new());
                }
                EventBatchItem::Private(private_event) => {
                    let event = events.last().ok_or_else(|| {
                        DbError::InvalidInput(
                            "private event requires a preceding public event in the batch".into(),
                        )
                    })?;
                    let attached = private_events
                        .last_mut()
                        .expect("public event has an attachment list");
                    if !matches!(event.kind, EventType::ToolResult(_)) {
                        return Err(DbError::InvalidInput(
                            "codemode private events require a tool result".into(),
                        ));
                    }
                    let (size, limit) = match private_event {
                        PrivateEvent::CodeModeSnapshot(snapshot) => {
                            (snapshot.len(), MAX_CODEMODE_SNAPSHOT_BYTES)
                        }
                        PrivateEvent::CodeModeOutput(text) => {
                            (text.len(), MAX_CODEMODE_OUTPUT_BYTES)
                        }
                    };
                    if size > limit {
                        return Err(DbError::InvalidInput(format!(
                            "{:?} exceeds {} MiB",
                            private_event.kind(),
                            limit / (1024 * 1024)
                        )));
                    }
                    if attached
                        .iter()
                        .any(|item| item.kind() == private_event.kind())
                    {
                        return Err(DbError::InvalidInput(format!(
                            "duplicate {:?} for event",
                            private_event.kind()
                        )));
                    }
                    attached.push(private_event);
                }
            }
        }
        self.add_events_inner(agent, &events, &private_events).await
    }

    async fn add_events_inner(
        &mut self,
        agent: AgentId,
        events: &[&NewEvent],
        private_events: &[Vec<&PrivateEvent>],
    ) -> Result<AddEventResult, DbError> {
        let mut savepoint = self.transaction.begin().await?;
        let stream = get_agent(&mut savepoint, agent).await?;
        if events.is_empty() {
            savepoint.commit().await?;
            return Ok(AddEventResult {
                events: Vec::new(),
                new_state: None,
                new_usage: None,
            });
        }

        validate_result_transitions(stream.state, events)?;

        let session = get_session(&mut savepoint, stream.session_id).await?;
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
            .execute(&mut *savepoint)
            .await?;
            insert_payload(
                &mut savepoint,
                result.last_insert_rowid(),
                agent_id,
                &payload,
            )
            .await?;
            for private_event in &private_events[offset] {
                match private_event {
                    PrivateEvent::CodeModeSnapshot(snapshot) => {
                        let stored = encode_snapshot(snapshot).await?;
                        sqlx::query(
                            "INSERT INTO codemode_snapshots (event_pk, snapshot) VALUES (?, ?)",
                        )
                        .bind(result.last_insert_rowid())
                        .bind(stored)
                        .execute(&mut *savepoint)
                        .await?;
                    }
                    PrivateEvent::CodeModeOutput(text) => {
                        sqlx::query(
                            "INSERT INTO codemode_outputs (event_pk, output) VALUES (?, ?)",
                        )
                        .bind(result.last_insert_rowid())
                        .bind(text.as_str())
                        .execute(&mut *savepoint)
                        .await?;
                    }
                }
            }
            saved.push(Event {
                id: EventId(decode_integer(seq)?),
                source_agent_id: agent,
                timestamp: decode_time(timestamp)?,
                kind: payload,
            });
        }

        let new_state = events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                EventType::StatusChange(change) => Some(change.new_status),
                _ => None,
            })
            .filter(|state| *state != stream.state);
        let mut usage = stream.usage.clone();
        for event in events {
            match &event.kind {
                EventType::InferenceResultEnd(end) => add_usage(&mut usage, &end.usage)?,
                EventType::DecisionCall(call) => add_decision_cost(&mut usage, call),
                // A summary billed on its own agent reports nothing here; adding
                // it would turn every known total into "unknown".
                EventType::Compaction(compaction) if !compaction.usage.is_unreported() => {
                    add_usage(&mut usage, &compaction.usage)?
                }
                _ => {}
            }
        }
        let new_usage = (usage != stream.usage).then_some(usage);
        let encoded_usage = new_usage.as_ref().map(serde_json::to_string).transpose()?;
        let state = new_state.map(encode_agent_state);
        let now = now_us()?;
        sqlx::query!(
            "UPDATE agents SET next_event_seq = ?, state = COALESCE(?, state), usage = COALESCE(?, usage), updated_at_us = ? WHERE id = ?",
            next_seq,
            state,
            encoded_usage,
            now,
            agent_id
        )
        .execute(&mut *savepoint)
        .await?;
        sqlx::query!(
            "UPDATE sessions SET next_position = ?, updated_at_us = ? WHERE id = ?",
            next_position,
            now,
            session_id
        )
        .execute(&mut *savepoint)
        .await?;

        savepoint.commit().await?;
        Ok(AddEventResult {
            events: saved,
            new_state,
            new_usage,
        })
    }
}

fn validate_result_transitions(mut state: AgentState, events: &[&NewEvent]) -> Result<(), DbError> {
    for (index, event) in events.iter().enumerate() {
        match &event.kind {
            EventType::Result(_) => {
                if state != AgentState::Running {
                    return Err(DbError::InvalidInput(
                        "a result requires a running agent".into(),
                    ));
                }
                if !matches!(
                    events.get(index + 1).map(|event| &event.kind),
                    Some(EventType::StatusChange(change)) if change.new_status == AgentState::Idle
                ) {
                    return Err(DbError::InvalidInput(
                        "a result must be immediately followed by an idle status in the same batch"
                            .into(),
                    ));
                }
                state = AgentState::Idle;
            }
            EventType::StatusChange(change) => state = change.new_status,
            _ => {}
        }
    }
    Ok(())
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

pub(super) async fn agent_state_at(
    conn: &mut SqliteConnection,
    agent: AgentId,
    through: EventId,
) -> Result<AgentState, DbError> {
    let segments = load_segments(conn, agent, EventId(0), Some(through)).await?;
    for (owner, lower, upper) in segments.into_iter().rev() {
        let row = sqlx::query!(
            "SELECT CASE WHEN e.kind = 'result' THEN 'idle' ELSE s.status END AS \"status!: String\" FROM events e LEFT JOIN event_status_change s ON s.event_pk = e.event_pk WHERE e.agent_id = ? AND e.event_seq BETWEEN ? AND ? AND e.kind IN ('status_change', 'result') ORDER BY e.event_seq DESC LIMIT 1",
            owner,
            lower,
            upper
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = row {
            return parse_agent_state(&row.status);
        }
    }

    Ok(AgentState::Idle)
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

pub(super) async fn load_segments(
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
        let Some(source_id) = current.forked_from else {
            break;
        };
        let parent = match get_agent(conn, source_id).await {
            Err(DbError::NotFound { .. }) => {
                return Err(DbError::Corrupt("missing agent parent".into()));
            }
            result => result?,
        };
        let owner = get_session(conn, current.session_id).await?;
        if owner.forked_from != Some(parent.session_id)
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

pub(super) fn add_usage(total: &mut Usage, usage: &llm_api::api::Usage) -> Result<(), DbError> {
    fn add(a: Option<u64>, b: Option<u64>) -> Result<Option<u64>, DbError> {
        match (a, b) {
            (Some(a), Some(b)) => {
                let sum = a
                    .checked_add(b)
                    .ok_or_else(|| DbError::InvalidInput("token usage overflow".into()))?;
                encode_integer(sum)?;
                Ok(Some(sum))
            }
            _ => Ok(None),
        }
    }
    total.input_tokens = add(total.input_tokens, usage.input_tokens)?;
    total.output_tokens = add(total.output_tokens, usage.output_tokens)?;
    total.total_tokens = add(total.total_tokens, usage.total_tokens)?;
    total.cached_input_tokens = add(total.cached_input_tokens, usage.cached_input_tokens)?;
    total.cache_write_tokens = add(total.cache_write_tokens, usage.cache_write_tokens)?;
    total.reasoning_output_tokens =
        add(total.reasoning_output_tokens, usage.reasoning_output_tokens)?;
    add_cost(total, usage.cost);
    Ok(())
}

/// Add a decision call's charge. A failed call produced no answer and reports
/// no charge, so it leaves the total known.
pub(super) fn add_decision_cost(total: &mut Usage, call: &DecisionCall) {
    if let Some(usage) = call.usage() {
        add_cost(total, usage.cost);
    }
}

fn add_cost(total: &mut Usage, cost: Option<f64>) {
    total.cost = total.cost.zip(cost).map(|(total, cost)| total + cost);
}

/// Event payload and timestamp awaiting a committed identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEvent {
    /// Timestamp to preserve when committing the event.
    pub timestamp: OffsetDateTime,
    /// Transcript event payload.
    pub kind: EventType,
}

impl NewEvent {
    /// Creates an uncommitted event stamped with the current UTC time.
    pub fn new(kind: EventType) -> Self {
        Self {
            timestamp: OffsetDateTime::now_utc(),
            kind,
        }
    }
}

/// Pagination bounds within an agent's event lineage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRange {
    /// Exclusive lower sequence bound; defaults to zero.
    pub after: EventId,
    /// Inclusive upper sequence bound; absent to use the current head.
    pub through: Option<EventId>,
    /// Maximum number of events to return; defaults to 1,000.
    pub limit: u32,
}

impl Default for EventRange {
    fn default() -> Self {
        Self {
            after: EventId(0),
            through: None,
            limit: 1000,
        }
    }
}
