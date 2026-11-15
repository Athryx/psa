//! Higher level convenience functions to return specific types or ranges of events.

use super::{
    DatabaseCursor, DbError,
    events::{add_decision_cost, add_usage, event_cutoff_at, load_event_rows, load_segments},
    payloads::parse_model,
    schema::{decode_integer, decode_time, encode_integer},
};
use crate::{
    agents::{AgentId, AgentUsage},
    sessions::SessionId,
};
use llm_api::providers::Model;
use psa_api::{DecisionCall, DecisionOutcome, EventEnvelope, ModelUsage, ResultRecord, events::*};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct InferenceResultEvents {
    pub start: Event,
    pub end: Event,
    pub items: Vec<Event>,
}

impl DatabaseCursor {
    pub async fn load_last_result(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<ResultRecord>, DbError> {
        Ok(self
            .load_last_results(session, agent, 1)
            .await?
            .into_iter()
            .next())
    }

    /// Newest first, including results visible through every bounded fork ancestor.
    pub async fn load_last_results(
        &mut self,
        session: SessionId,
        agent: AgentId,
        limit: u32,
    ) -> Result<Vec<ResultRecord>, DbError> {
        self.require_agent_session(session, agent).await?;
        let envelopes = load_result_envelopes(
            self.connection(),
            session,
            agent,
            EventRange {
                after: EventId(0),
                through: None,
                limit,
            },
            true,
        )
        .await?;
        envelopes
            .into_iter()
            .map(|envelope| {
                let EventType::Result(value) = envelope.event.kind else {
                    return Err(DbError::Corrupt(
                        "result query returned another event kind".into(),
                    ));
                };
                Ok(ResultRecord {
                    session_id: session,
                    agent_id: agent,
                    event_id: envelope.event.id,
                    source_agent_id: envelope.event.source_agent_id,
                    position: envelope.position,
                    timestamp: envelope.event.timestamp,
                    value,
                })
            })
            .collect()
    }

    /// Filters before pagination; event IDs remain IDs in the full visible history.
    pub async fn load_agent_result_envelopes(
        &mut self,
        session: SessionId,
        agent: AgentId,
        range: EventRange,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.require_agent_session(session, agent).await?;
        load_result_envelopes(self.connection(), session, agent, range, false).await
    }

    /// Usage per model across the bounded fork lineage, sorted by model slug.
    pub async fn load_agent_model_usage(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Vec<ModelUsage>, DbError> {
        let metadata = self.require_agent_session(session, agent).await?;
        let Some(through) = metadata.next_event_id.0.checked_sub(1).filter(|id| *id > 0) else {
            return Ok(Vec::new());
        };
        model_usage_at(self.connection(), agent, EventId(through)).await
    }

    /// Project status from the newest relevant markers across the bounded fork lineage.
    pub async fn load_agent_status(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<psa_api::AgentStatusSnapshot, DbError> {
        let metadata = self.require_agent_session(session, agent).await?;
        let mut markers = Vec::new();
        for kind in ["new_context", "model", "compaction", "inference_end"] {
            if let Some(id) = self.find_last_marker(agent, kind, None).await? {
                markers.push(id);
            }
        }
        markers.sort_unstable();
        let mut status = psa_api::AgentStatusSnapshot::default();
        for id in markers {
            let events = self
                .load_events(
                    agent,
                    EventRange {
                        after: EventId(id - 1),
                        through: Some(EventId(id)),
                        limit: 1,
                    },
                )
                .await?;
            if let Some(event) = events.first() {
                status.apply_event(event);
            }
        }
        status.head = metadata.next_event_id.0.saturating_sub(1);
        Ok(status)
    }

    /// Includes staged appends in this cursor. Never silently truncates a window.
    pub async fn load_all_events(&mut self, agent: AgentId) -> Result<Vec<Event>, DbError> {
        self.load_event_window(agent, EventId(0), None).await
    }

    pub async fn load_context_window(&mut self, agent: AgentId) -> Result<Vec<Event>, DbError> {
        let first = self.find_last_marker(agent, "new_context", None).await?;
        self.load_event_window(agent, EventId(first.unwrap_or(1) - 1), None)
            .await
    }

    pub async fn load_events_since_last_inference(
        &mut self,
        agent: AgentId,
    ) -> Result<Option<Vec<Event>>, DbError> {
        let Some(first) = self
            .find_last_marker(agent, "inference_start", None)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            self.load_event_window(agent, EventId(first - 1), None)
                .await?,
        ))
    }

    pub async fn load_last_inference_result(
        &mut self,
        agent: AgentId,
    ) -> Result<Option<InferenceResultEvents>, DbError> {
        let Some(events) = self.load_events_since_last_inference(agent).await? else {
            return Ok(None);
        };
        let mut events = events.into_iter();
        let start = events
            .next()
            .ok_or_else(|| DbError::Corrupt("missing inference start".into()))?;
        let EventType::InferenceResultStart(ref marker) = start.kind else {
            return Err(DbError::Corrupt("invalid inference start".into()));
        };
        let id = marker.id;
        let mut items = Vec::new();
        for event in events {
            match &event.kind {
                EventType::InferenceResultEnd(end) if end.id == id => {
                    return Ok(Some(InferenceResultEvents {
                        start,
                        end: event,
                        items,
                    }));
                }
                EventType::NewContext => return Err(DbError::IncompleteInference(id)),
                EventType::InferenceResultStart(_) | EventType::InferenceResultEnd(_) => {
                    return Err(DbError::Corrupt("mismatched inference delimiters".into()));
                }
                _ => items.push(event),
            }
        }
        Err(DbError::IncompleteInference(id))
    }

    pub(super) async fn load_event_window(
        &mut self,
        agent: AgentId,
        mut after: EventId,
        through: Option<EventId>,
    ) -> Result<Vec<Event>, DbError> {
        let head = self.get_agent(agent).await?.next_event_id.0 - 1;
        let through = EventId(through.map_or(head, |id| id.0.min(head)));
        let mut events = Vec::new();
        while after < through {
            let page = self
                .load_events(
                    agent,
                    EventRange {
                        after,
                        through: Some(through),
                        limit: 10_000,
                    },
                )
                .await?;
            let Some(last) = page.last() else {
                break;
            };
            after = last.id;
            events.extend(page);
        }
        Ok(events)
    }

    /// The NewContext event opening the model context that contains `through`.
    /// `None` means that context begins with the agent's first event.
    pub async fn find_context_start(
        &mut self,
        session: SessionId,
        agent: AgentId,
        through: EventId,
    ) -> Result<Option<EventId>, DbError> {
        self.require_agent_session(session, agent).await?;
        let marker = self
            .find_last_marker(agent, "new_context", Some(through))
            .await?;
        Ok(marker.map(EventId))
    }

    async fn find_last_marker(
        &mut self,
        agent: AgentId,
        kind: &str,
        through: Option<EventId>,
    ) -> Result<Option<u64>, DbError> {
        for (owner, lower, upper) in load_segments(self.connection(), agent, EventId(0), through)
            .await?
            .into_iter()
            .rev()
        {
            let row = sqlx::query!("SELECT MAX(event_seq) AS seq FROM events WHERE agent_id = ? AND event_seq BETWEEN ? AND ? AND kind = ?", owner, lower, upper, kind)
                .fetch_one(self.connection()).await?;
            if let Some(seq) = row.seq {
                return Ok(Some(decode_integer(seq)?));
            }
        }
        Ok(None)
    }

    pub async fn require_agent_session(
        &mut self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<crate::agents::Agent, DbError> {
        let agent = self.get_agent(agent).await?;
        if agent.session_id != session {
            return Err(DbError::NotFound {
                entity: "agent in session",
                id: agent.id.0,
            });
        }
        Ok(agent)
    }

    pub async fn load_agent_envelopes(
        &mut self,
        session: SessionId,
        agent: AgentId,
        range: EventRange,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.require_agent_session(session, agent).await?;
        let events = self.load_events(agent, range.clone()).await?;
        let rows = load_event_rows(self.connection(), agent, &range).await?;
        events
            .into_iter()
            .zip(rows)
            .map(|(event, row)| {
                Ok(EventEnvelope {
                    session_id: session,
                    agent_id: agent,
                    position: decode_integer(row.session_position)?,
                    event,
                })
            })
            .collect()
    }

    /// Bounded reverse history across the logical lineage, returned chronologically.
    pub async fn load_recent_agent_events(
        &mut self,
        session: SessionId,
        agent: AgentId,
        through: u64,
        limit: u32,
        results_only: bool,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.require_agent_session(session, agent).await?;
        if results_only {
            let mut events = load_result_envelopes(
                self.connection(),
                session,
                agent,
                EventRange {
                    after: EventId(0),
                    through: Some(EventId(through)),
                    limit,
                },
                true,
            )
            .await?;
            events.reverse();
            return Ok(events);
        }
        let segments =
            load_segments(self.connection(), agent, EventId(0), Some(EventId(through))).await?;
        let mut remaining = u64::from(limit);
        let mut lower = through;
        for (_, first, last) in segments.into_iter().rev() {
            let count = (last - first + 1) as u64;
            let take = count.min(remaining);
            lower = last as u64 + 1 - take;
            remaining -= take;
            if remaining == 0 {
                break;
            }
        }
        self.load_agent_envelopes(
            session,
            agent,
            EventRange {
                after: EventId(lower.saturating_sub(1)),
                through: Some(EventId(through)),
                limit,
            },
        )
        .await
    }

    /// Session positions include agent creation and can therefore have gaps.
    pub async fn load_recent_session_events(
        &mut self,
        session: SessionId,
        through: u64,
        limit: u32,
        results_only: bool,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.get_session(session).await?;
        let mut events = Vec::new();
        for agent in self.list_agents(session).await? {
            let upper =
                decode_integer(event_cutoff_at(self.connection(), agent.id, through).await?)?;
            events.extend(
                self.load_recent_agent_events(session, agent.id, upper, limit, results_only)
                    .await?,
            );
        }
        events.sort_by_key(|event| std::cmp::Reverse(event.position));
        events.truncate(limit as usize);
        events.reverse();
        Ok(events)
    }

    /// Shared session positions include agent creation; event IDs remain per stream.
    pub async fn load_session_events(
        &mut self,
        session: SessionId,
        after: u64,
        through: Option<u64>,
        limit: u32,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.load_session_event_page(session, after, through, limit, false)
            .await
    }

    pub async fn load_session_result_events(
        &mut self,
        session: SessionId,
        after: u64,
        through: Option<u64>,
        limit: u32,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        self.load_session_event_page(session, after, through, limit, true)
            .await
    }

    async fn load_session_event_page(
        &mut self,
        session: SessionId,
        after: u64,
        through: Option<u64>,
        limit: u32,
        results_only: bool,
    ) -> Result<Vec<EventEnvelope>, DbError> {
        if limit == 0 || limit > 10_000 {
            return Err(DbError::InvalidInput(
                "event page size must be between 1 and 10000".into(),
            ));
        }
        let head = self.get_session(session).await?.next_position - 1;
        let through = through.unwrap_or(head).min(head);
        if after >= through {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        for agent in self.list_agents(session).await? {
            let lower = event_cutoff_at(self.connection(), agent.id, after).await?;
            let upper = event_cutoff_at(self.connection(), agent.id, through).await?;
            let range = EventRange {
                after: EventId(decode_integer(lower)?),
                through: Some(EventId(decode_integer(upper)?)),
                limit,
            };
            let page = if results_only {
                self.load_agent_result_envelopes(session, agent.id, range)
                    .await?
            } else {
                self.load_agent_envelopes(session, agent.id, range).await?
            };
            events.extend(page);
        }
        events.sort_by_key(|event| event.position);
        events.truncate(limit as usize);
        Ok(events)
    }

    /// Recomputes stored totals left unknown by compactions that reported no
    /// usage before such records were skipped. Fully known totals are untouched.
    /// Returns how many agents were repaired.
    pub async fn repair_unknown_usage(&mut self) -> Result<usize, DbError> {
        let mut repaired = 0;
        for agent in self.list_all_agents().await? {
            let usage = &agent.usage;
            if usage.input_tokens.is_some()
                && usage.output_tokens.is_some()
                && usage.total_tokens.is_some()
            {
                continue;
            }
            let Some(through) = agent.next_event_id.0.checked_sub(1).filter(|id| *id > 0) else {
                continue;
            };

            let recomputed = usage_at(self.connection(), agent.id, EventId(through)).await?;
            if recomputed == agent.usage {
                continue;
            }
            let id = encode_integer(agent.id.0)?;
            sqlx::query("UPDATE agents SET usage = ? WHERE id = ?")
                .bind(serde_json::to_string(&recomputed)?)
                .bind(id)
                .execute(self.connection())
                .await?;
            repaired += 1;
        }
        Ok(repaired)
    }

    pub async fn list_all_agents(&mut self) -> Result<Vec<crate::agents::Agent>, DbError> {
        let rows = sqlx::query!("SELECT id FROM agents ORDER BY id")
            .fetch_all(self.connection())
            .await?;
        let mut agents = Vec::new();
        for row in rows {
            agents.push(self.get_agent(AgentId(decode_integer(row.id)?)).await?);
        }
        Ok(agents)
    }
}

struct ResultRow {
    agent_id: i64,
    event_seq: i64,
    session_position: i64,
    timestamp_us: i64,
    value: String,
}

async fn load_result_envelopes(
    conn: &mut SqliteConnection,
    session: SessionId,
    agent: AgentId,
    range: EventRange,
    newest_first: bool,
) -> Result<Vec<EventEnvelope>, DbError> {
    if range.limit == 0 || range.limit > 10_000 {
        return Err(DbError::InvalidInput(
            "result page size must be between 1 and 10000".into(),
        ));
    }
    let mut segments = load_segments(conn, agent, range.after, range.through).await?;
    if newest_first {
        segments.reverse();
    }
    let mut rows = Vec::new();
    for (owner, lower, upper) in segments {
        let remaining = i64::from(range.limit) - rows.len() as i64;
        if remaining == 0 {
            break;
        }
        let page = if newest_first {
            sqlx::query_as!(ResultRow,
                "SELECT e.agent_id, e.event_seq, e.session_position, e.timestamp_us, p.value FROM events e JOIN event_result p ON p.event_pk=e.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ? AND e.kind='result' ORDER BY e.event_seq DESC LIMIT ?",
                owner, lower, upper, remaining
            ).fetch_all(&mut *conn).await?
        } else {
            sqlx::query_as!(ResultRow,
                "SELECT e.agent_id, e.event_seq, e.session_position, e.timestamp_us, p.value FROM events e JOIN event_result p ON p.event_pk=e.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ? AND e.kind='result' ORDER BY e.event_seq ASC LIMIT ?",
                owner, lower, upper, remaining
            ).fetch_all(&mut *conn).await?
        };
        rows.extend(page);
    }
    rows.into_iter()
        .map(|row| {
            Ok(EventEnvelope {
                session_id: session,
                agent_id: agent,
                position: decode_integer(row.session_position)?,
                event: Event {
                    id: EventId(decode_integer(row.event_seq)?),
                    source_agent_id: AgentId(decode_integer(row.agent_id)?),
                    timestamp: decode_time(row.timestamp_us)?,
                    kind: EventType::Result(serde_json::from_str(&row.value)?),
                },
            })
        })
        .collect()
}

pub(super) async fn usage_at(
    conn: &mut SqliteConnection,
    agent: AgentId,
    through: EventId,
) -> Result<AgentUsage, DbError> {
    let mut total = AgentUsage::default();
    for (owner, lower, upper) in load_segments(conn, agent, EventId(0), Some(through)).await? {
        let rows = sqlx::query(
            "SELECT p.input_tokens, p.output_tokens, p.total_tokens, p.cached_tokens, p.cache_write_tokens, p.reasoning_tokens, p.cost, p.usage_details FROM event_inference_result_end p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ? UNION ALL SELECT p.input_tokens, p.output_tokens, p.total_tokens, p.cached_tokens, p.cache_write_tokens, p.reasoning_tokens, p.cost, p.usage_details FROM event_compaction p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ? AND (p.input_tokens IS NOT NULL OR p.output_tokens IS NOT NULL OR p.total_tokens IS NOT NULL OR p.cached_tokens IS NOT NULL OR p.cache_write_tokens IS NOT NULL OR p.reasoning_tokens IS NOT NULL OR p.cost IS NOT NULL OR p.usage_details <> '{}')",
        ).bind(owner).bind(lower).bind(upper).bind(owner).bind(lower).bind(upper).fetch_all(&mut *conn).await?;
        for row in rows {
            add_usage(&mut total, &parse_usage_row(&row)?)?;
        }
        for call in load_decision_calls(conn, owner, lower, upper).await? {
            add_decision_cost(&mut total, &call);
        }
    }
    // Validate the final known totals against SQLite's integer domain as well.
    for count in [
        total.input_tokens,
        total.output_tokens,
        total.total_tokens,
        total.cached_input_tokens,
        total.cache_write_tokens,
        total.reasoning_output_tokens,
    ]
    .into_iter()
    .flatten()
    {
        encode_integer(count)?;
    }
    Ok(total)
}

/// Usage per model over the bounded fork lineage, sorted by model slug.
pub(super) async fn model_usage_at(
    conn: &mut SqliteConnection,
    agent: AgentId,
    through: EventId,
) -> Result<Vec<ModelUsage>, DbError> {
    let mut totals = BTreeMap::new();
    for (owner, lower, upper) in load_segments(conn, agent, EventId(0), Some(through)).await? {
        // Inference IDs are the start event's sequence, unique within an agent.
        let rows = sqlx::query(
            "SELECT s.model_provider, s.model_name, s.model_effort, p.input_tokens, p.output_tokens, p.total_tokens, p.cached_tokens, p.cache_write_tokens, p.reasoning_tokens, p.cost, p.usage_details FROM event_inference_result_end p JOIN events e ON e.event_pk=p.event_pk JOIN event_inference_result_start s ON s.inference_id=p.inference_id JOIN events es ON es.event_pk=s.event_pk AND es.agent_id=e.agent_id WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
        ).bind(owner).bind(lower).bind(upper).fetch_all(&mut *conn).await?;
        for row in rows {
            let (Some(provider), Some(name), Some(effort)) = (
                row.try_get::<Option<String>, _>("model_provider")?,
                row.try_get::<Option<String>, _>("model_name")?,
                row.try_get::<Option<String>, _>("model_effort")?,
            ) else {
                return Err(DbError::Corrupt(
                    "result start has no model snapshot".into(),
                ));
            };
            let model = parse_model(&provider, name, &effort)?.model;
            add_usage(model_entry(&mut totals, model), &parse_usage_row(&row)?)?;
        }

        for call in load_decision_calls(conn, owner, lower, upper).await? {
            if matches!(call.outcome, DecisionOutcome::Failed(_)) {
                continue;
            }
            let usage = llm_api::api::Usage {
                input_tokens: call.usage.input_tokens,
                cached_input_tokens: call.usage.cached_tokens,
                cost: call.usage.cost,
                ..Default::default()
            };
            add_usage(model_entry(&mut totals, call.model), &usage)?;
        }
    }
    Ok(totals.into_values().collect())
}

fn model_entry(totals: &mut BTreeMap<String, ModelUsage>, model: Model) -> &mut AgentUsage {
    &mut totals
        .entry(model.slug())
        .or_insert_with(|| ModelUsage {
            model,
            usage: AgentUsage::default(),
        })
        .usage
}

fn parse_usage_row(row: &SqliteRow) -> Result<llm_api::api::Usage, DbError> {
    let count = |column: &str| -> Result<Option<u64>, DbError> {
        row.try_get::<Option<i64>, _>(column)?
            .map(decode_integer)
            .transpose()
    };
    Ok(llm_api::api::Usage {
        input_tokens: count("input_tokens")?,
        output_tokens: count("output_tokens")?,
        total_tokens: count("total_tokens")?,
        cached_input_tokens: count("cached_tokens")?,
        cache_write_input_tokens: count("cache_write_tokens")?,
        reasoning_output_tokens: count("reasoning_tokens")?,
        cost: row.try_get("cost")?,
        provider_details: serde_json::from_str(row.try_get("usage_details")?)?,
    })
}

async fn load_decision_calls(
    conn: &mut SqliteConnection,
    owner: i64,
    lower: i64,
    upper: i64,
) -> Result<Vec<DecisionCall>, DbError> {
    let calls = sqlx::query_scalar::<_, String>(
        "SELECT p.value FROM event_decision_call p JOIN events e ON e.event_pk=p.event_pk WHERE e.agent_id=? AND e.event_seq BETWEEN ? AND ?",
    ).bind(owner).bind(lower).bind(upper).fetch_all(&mut *conn).await?;
    calls
        .iter()
        .map(|call| Ok(serde_json::from_str(call)?))
        .collect()
}
