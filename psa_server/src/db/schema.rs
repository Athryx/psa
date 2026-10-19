//! SQLite row types and checked conversions. Not part of the domain API.

use super::DbError;
use crate::{
    agents::{Agent, AgentId, AgentState, events::EventId},
    sessions::{Session, SessionId},
};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

pub(super) struct SessionRow {
    pub id: i64,
    pub parent_session_id: Option<i64>,
    pub first_position: i64,
    pub next_position: i64,
    pub name: String,
    pub working_directory: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

impl TryFrom<SessionRow> for Session {
    type Error = DbError;
    fn try_from(row: SessionRow) -> Result<Self, DbError> {
        Ok(Self {
            id: SessionId(decode_integer(row.id)?),
            parent_id: row
                .parent_session_id
                .map(decode_integer)
                .transpose()?
                .map(SessionId),
            first_position: decode_integer(row.first_position)?,
            next_position: decode_integer(row.next_position)?,
            name: row.name,
            working_directory: PathBuf::from(row.working_directory),
            created_at: decode_time(row.created_at_us)?,
            updated_at: decode_time(row.updated_at_us)?,
        })
    }
}

pub(super) struct AgentRow {
    pub id: i64,
    pub session_id: i64,
    pub parent_agent_id: Option<i64>,
    pub first_event_seq: i64,
    pub next_event_seq: i64,
    pub created_position: i64,
    pub name: String,
    pub state: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

impl TryFrom<AgentRow> for Agent {
    type Error = DbError;
    fn try_from(row: AgentRow) -> Result<Self, DbError> {
        Ok(Self {
            id: AgentId(decode_integer(row.id)?),
            session_id: SessionId(decode_integer(row.session_id)?),
            parent_id: row
                .parent_agent_id
                .map(decode_integer)
                .transpose()?
                .map(AgentId),
            first_event_id: EventId(decode_integer(row.first_event_seq)?),
            next_event_id: EventId(decode_integer(row.next_event_seq)?),
            created_position: decode_integer(row.created_position)?,
            name: row.name,
            state: parse_agent_state(&row.state)?,
            created_at: decode_time(row.created_at_us)?,
            updated_at: decode_time(row.updated_at_us)?,
        })
    }
}

pub(super) struct EventRow {
    pub event_pk: i64,
    pub agent_id: i64,
    pub event_seq: i64,
    pub session_position: i64,
    pub timestamp_us: i64,
    pub kind: String,
}

pub(super) fn encode_agent_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Running => "running",
        AgentState::Interrupted => "interrupted",
    }
}

pub(super) fn parse_agent_state(state: &str) -> Result<AgentState, DbError> {
    match state {
        "idle" => Ok(AgentState::Idle),
        "running" => Ok(AgentState::Running),
        "interrupted" => Ok(AgentState::Interrupted),
        _ => Err(DbError::Corrupt(format!("invalid agent state: {state}"))),
    }
}

pub(super) fn encode_integer(value: u64) -> Result<i64, DbError> {
    i64::try_from(value)
        .map_err(|_| DbError::InvalidInput("integer exceeds SQLite's signed 64-bit range".into()))
}

pub(super) fn decode_integer(value: i64) -> Result<u64, DbError> {
    u64::try_from(value).map_err(|_| DbError::Corrupt("negative unsigned value".into()))
}

pub(super) fn encode_time(value: OffsetDateTime) -> Result<i64, DbError> {
    i64::try_from(value.unix_timestamp_nanos().div_euclid(1000))
        .map_err(|_| DbError::InvalidInput("timestamp exceeds SQLite range".into()))
}

pub(super) fn decode_time(value: i64) -> Result<OffsetDateTime, DbError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value) * 1000)
        .map_err(|error| DbError::Corrupt(format!("invalid timestamp: {error}")))
}

pub(super) fn now_us() -> Result<i64, DbError> {
    encode_time(OffsetDateTime::now_utc())
}

pub(super) fn encode_path(path: &Path) -> Result<&str, DbError> {
    let value = path
        .to_str()
        .ok_or_else(|| DbError::InvalidInput("working directory must be UTF-8".into()))?;
    if value.is_empty() {
        return Err(DbError::InvalidInput(
            "working directory must not be empty".into(),
        ));
    }
    Ok(value)
}

pub(super) fn validate_name(name: &str) -> Result<(), DbError> {
    if name.trim().is_empty() {
        return Err(DbError::InvalidInput("name must not be empty".into()));
    }
    Ok(())
}
