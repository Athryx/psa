//! Prompt-cache accounting kept beside an agent's usage totals.
//!
//! A request can only hit the cache on input an earlier request already sent,
//! so the hit rate is measured against that repeated input rather than all
//! input. Tracking lives in the agent's stored usage JSON, next to the totals.
use super::{
    DbError, WriteCursor,
    events::load_segments,
    queries::parse_usage_row,
    schema::{decode_integer, encode_integer},
};
use crate::agents::AgentId;
use llm_api::api::Usage;
use psa::api::events::{EventId, EventType};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheTracking {
    /// Input each request repeated from the previous request in its context,
    /// or its cached input if larger. Unknown (`None`) once a request's input
    /// went unreported.
    pub cacheable_input_tokens: Option<u64>,
    /// Input of the latest request in the current context: the prefix the next
    /// request can repeat. `None` before a context's first request.
    pub previous_input_tokens: Option<u64>,
}

impl CacheTracking {
    /// Tracking for an agent with no requests yet.
    pub fn start() -> Self {
        Self {
            cacheable_input_tokens: Some(0),
            previous_input_tokens: None,
        }
    }

    pub fn apply_event(&mut self, kind: &EventType) -> Result<(), DbError> {
        match kind {
            EventType::InferenceResultEnd(end) => self.apply_request(&end.usage)?,
            EventType::Compaction(compaction) => self.apply_compaction(&compaction.usage)?,
            EventType::NewContext => self.previous_input_tokens = None,
            _ => {}
        }
        Ok(())
    }

    /// A summary request repeats the conversation, then replaces it. Summaries
    /// billed on another agent report nothing here.
    fn apply_compaction(&mut self, usage: &Usage) -> Result<(), DbError> {
        if !usage.is_unreported() {
            self.apply_request(usage)?;
        }
        self.previous_input_tokens = None;
        Ok(())
    }

    fn apply_request(&mut self, usage: &Usage) -> Result<(), DbError> {
        let Some(input) = usage.input_tokens else {
            self.cacheable_input_tokens = None;
            self.previous_input_tokens = None;
            return Ok(());
        };
        let repeated = self.previous_input_tokens.unwrap_or(0).min(input);
        // A cache shared beyond this agent, such as for a common system prompt,
        // can serve a context's first request too.
        let cached = usage.cached_input_tokens.unwrap_or(0).min(input);
        self.cacheable_input_tokens = self
            .cacheable_input_tokens
            .map(|total| {
                total
                    .checked_add(repeated.max(cached))
                    .ok_or_else(|| DbError::InvalidInput("token usage overflow".into()))
            })
            .transpose()?;
        if let Some(total) = self.cacheable_input_tokens {
            encode_integer(total)?;
        }
        self.previous_input_tokens = Some(input);
        Ok(())
    }
}

/// The stored form of an agent's usage column. Rows written before cache
/// tracking have no `cache` entry.
#[derive(Serialize, Deserialize)]
struct StoredUsage {
    #[serde(flatten)]
    usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache: Option<CacheTracking>,
}

pub(super) fn parse_stored_usage(text: &str) -> Result<(Usage, Option<CacheTracking>), DbError> {
    let stored: StoredUsage = serde_json::from_str(text)?;
    Ok((stored.usage, stored.cache))
}

pub(super) fn encode_stored_usage(usage: &Usage, cache: CacheTracking) -> Result<String, DbError> {
    Ok(serde_json::to_string(&StoredUsage {
        usage: usage.clone(),
        cache: Some(cache),
    })?)
}

/// Cache tracking over the agent's fork lineage, through an inclusive event.
pub(super) async fn cache_tracking_at(
    conn: &mut SqliteConnection,
    agent: AgentId,
    through: EventId,
) -> Result<CacheTracking, DbError> {
    let mut cache = CacheTracking::start();
    for (owner, lower, upper) in load_segments(conn, agent, EventId(0), Some(through)).await? {
        let rows = sqlx::query(
            "SELECT e.kind, \
             COALESCE(p.input_tokens, c.input_tokens) AS input_tokens, \
             COALESCE(p.output_tokens, c.output_tokens) AS output_tokens, \
             COALESCE(p.total_tokens, c.total_tokens) AS total_tokens, \
             COALESCE(p.cached_tokens, c.cached_tokens) AS cached_tokens, \
             COALESCE(p.cache_write_tokens, c.cache_write_tokens) AS cache_write_tokens, \
             COALESCE(p.reasoning_tokens, c.reasoning_tokens) AS reasoning_tokens, \
             COALESCE(p.cost, c.cost) AS cost, \
             COALESCE(p.usage_details, c.usage_details, '{}') AS usage_details \
             FROM events e \
             LEFT JOIN event_inference_result_end p ON p.event_pk=e.event_pk \
             LEFT JOIN event_compaction c ON c.event_pk=e.event_pk \
             WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ? \
             AND e.kind IN ('inference_end', 'compaction', 'new_context') \
             ORDER BY e.event_seq",
        )
        .bind(owner)
        .bind(lower)
        .bind(upper)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            let kind: String = row.try_get("kind")?;
            match kind.as_str() {
                "new_context" => cache.apply_event(&EventType::NewContext)?,
                "compaction" => cache.apply_compaction(&parse_usage_row(&row)?)?,
                _ => cache.apply_request(&parse_usage_row(&row)?)?,
            }
        }
    }
    Ok(cache)
}

impl WriteCursor {
    /// Computes cache tracking for agents stored before it existed. Returns how
    /// many agents were backfilled.
    pub async fn backfill_cache_tracking(&mut self) -> Result<usize, DbError> {
        let rows = sqlx::query("SELECT id, next_event_seq, usage FROM agents")
            .fetch_all(self.connection())
            .await?;
        let mut backfilled = 0;
        for row in rows {
            let (usage, cache) = parse_stored_usage(row.try_get("usage")?)?;
            if cache.is_some() {
                continue;
            }
            let id: i64 = row.try_get("id")?;
            let next = decode_integer(row.try_get("next_event_seq")?)?;
            let cache = match next.checked_sub(1).filter(|through| *through > 0) {
                Some(through) => {
                    let agent = AgentId(decode_integer(id)?);
                    cache_tracking_at(self.connection(), agent, EventId(through)).await?
                }
                None => CacheTracking::start(),
            };
            sqlx::query("UPDATE agents SET usage = ? WHERE id = ?")
                .bind(encode_stored_usage(&usage, cache)?)
                .bind(id)
                .execute(self.connection())
                .await?;
            backfilled += 1;
        }
        Ok(backfilled)
    }
}
