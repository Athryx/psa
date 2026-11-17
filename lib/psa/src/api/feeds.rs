//! Event pages and live server-sent streams for transcripts and session summaries.

use crate::api::{
    Agent, AgentId, ClientQueryId, ClientQueryRequest, Session, SessionId,
    events::{Event, EventType},
};
use serde::{Deserialize, Serialize};

/// Which events a page or stream covers. Cursors are event IDs on agent routes
/// and session positions on session routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSpan {
    /// Events after the exclusive cursor `after`, up to the inclusive `through`
    /// or the current head. Streams accept only this span; without `through`
    /// they keep following new events.
    Forward {
        /// Exclusive lower cursor.
        after: u64,
        /// Inclusive upper cursor; `None` uses the current head.
        through: Option<u64>,
    },
    /// The newest page of history.
    Latest,
    /// The page of history ending just before this exclusive cursor.
    Before(u64),
    /// From the start of the model context containing `through` up to `through`:
    /// the page opens with that context's NewContext event, or the agent's first
    /// event. Agent routes only. Continue a partial page with
    /// `Forward { after: next_after, through }`.
    ContextOf {
        /// Inclusive upper cursor whose context is loaded.
        through: u64,
    },
}

/// Options shared by event pages and streams.
///
/// Sent as flat query parameters (`after`, `through`, `recent`, `before`,
/// `from_context_start`, ...); decoding rejects combinations that do not form
/// one [`EventSpan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EventsQueryParams", into = "EventsQueryParams")]
pub struct EventsQuery {
    /// Which events to return.
    pub span: EventSpan,
    /// Maximum page size, from 1 through 10,000; defaults to 1,000.
    pub limit: u32,
    /// Include provisional events.
    pub include_pending: bool,
    /// Return only durable result events. Cursors still count every event.
    pub results_only: bool,
}

impl Default for EventsQuery {
    fn default() -> Self {
        Self {
            span: EventSpan::Forward {
                after: 0,
                through: None,
            },
            limit: 1000,
            include_pending: false,
            results_only: false,
        }
    }
}

/// Flat wire form of [`EventsQuery`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EventsQueryParams {
    #[serde(skip_serializing_if = "is_zero")]
    after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    through: Option<u64>,
    #[serde(skip_serializing_if = "is_false")]
    recent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<u64>,
    #[serde(skip_serializing_if = "is_false")]
    from_context_start: bool,
    limit: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    include_pending: bool,
    #[serde(skip_serializing_if = "is_false")]
    results_only: bool,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl TryFrom<EventsQueryParams> for EventsQuery {
    type Error = String;

    fn try_from(params: EventsQueryParams) -> Result<Self, String> {
        let forward_bounds = params.after != 0 || params.through.is_some();
        let span = match (params.recent, params.before, params.from_context_start) {
            (false, None, false) => {
                if params.through.is_some_and(|through| through < params.after) {
                    return Err("through must not be before after".into());
                }
                EventSpan::Forward {
                    after: params.after,
                    through: params.through,
                }
            }
            (true, None, false) if !forward_bounds => EventSpan::Latest,
            (false, Some(before), false) if !forward_bounds => EventSpan::Before(before),
            (false, None, true) if params.after == 0 => match params.through {
                Some(through) => EventSpan::ContextOf { through },
                None => return Err("from_context_start requires through".into()),
            },
            _ => {
                return Err(
                    "recent, before, and from_context_start exclude each other and forward bounds"
                        .into(),
                );
            }
        };
        Ok(Self {
            span,
            limit: params.limit.unwrap_or(1000),
            include_pending: params.include_pending,
            results_only: params.results_only,
        })
    }
}

impl From<EventsQuery> for EventsQueryParams {
    fn from(query: EventsQuery) -> Self {
        let mut params = Self {
            limit: Some(query.limit),
            include_pending: query.include_pending,
            results_only: query.results_only,
            ..Self::default()
        };
        match query.span {
            EventSpan::Forward { after, through } => {
                params.after = after;
                params.through = through;
            }
            EventSpan::Latest => params.recent = true,
            EventSpan::Before(before) => params.before = Some(before),
            EventSpan::ContextOf { through } => {
                params.from_context_start = true;
                params.through = Some(through);
            }
        }
        params
    }
}

/// A transcript event located in a session's view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Session in which this event is viewed.
    pub session_id: SessionId,
    /// Agent as viewed in this session, distinct from event.source_agent_id.
    pub agent_id: AgentId,
    /// Session-wide event position used by session event cursors.
    pub position: u64,
    /// Immutable transcript event.
    pub event: Event,
}

/// A provisional transcript item that may be updated, committed, or removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEvent {
    /// Identifier of this provisional item.
    pub id: String,
    /// Revision of this provisional item's contents.
    pub revision: u64,
    /// Session containing the provisional item.
    pub session_id: SessionId,
    /// Agent publishing the provisional item.
    pub agent_id: AgentId,
    /// Current provisional event payload.
    pub kind: EventType,
}

/// A transcript page with live provisional state and pagination cursors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsPage {
    /// Committed events in cursor order.
    pub events: Vec<EventEnvelope>,
    /// Current provisional items when requested.
    pub pending: Vec<PendingEvent>,
    /// Outstanding client queries for the requested scope.
    pub queries: Vec<ClientQueryRequest>,
    /// Exclusive cursor to use for the next page.
    pub next_after: u64,
    /// Exclusive cursor for older history, absent when no older events remain.
    pub next_before: Option<u64>,
    /// Whether older committed events exist before this page.
    pub has_older: bool,
    /// Captured head cursor, clamped to the requested upper bound.
    pub head: u64,
    /// Whether additional committed events remain before the captured head.
    pub has_more: bool,
}

/// A server-sent notification for transcript or runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// Marks the end of historical replay and supplies current live state.
    Snapshot {
        /// Captured committed head cursor.
        cursor: u64,
        /// Current provisional items, when requested.
        pending: Vec<PendingEvent>,
        /// Outstanding client queries.
        queries: Vec<ClientQueryRequest>,
    },
    /// Publishes a client query awaiting a response.
    ClientQueryRequested {
        /// Query that clients may answer.
        query: ClientQueryRequest,
    },
    /// Removes a client query that is no longer outstanding.
    ClientQueryRemoved {
        /// Session containing the query.
        session_id: SessionId,
        /// Agent that issued the query.
        agent_id: AgentId,
        /// Identifier of the removed query.
        query_id: ClientQueryId,
    },
    /// Creates or replaces a provisional event.
    Upsert {
        /// Latest provisional event revision.
        pending: PendingEvent,
    },
    /// Publishes a committed transcript event.
    Committed {
        /// Committed event and its session location.
        event: EventEnvelope,
        /// Provisional identifier replaced by this commit, if any.
        pending_id: Option<String>,
    },
    /// Removes a provisional event without committing it.
    Removed {
        /// Session containing the provisional item.
        session_id: SessionId,
        /// Agent that published the provisional item.
        agent_id: AgentId,
        /// Identifier of the removed provisional item.
        id: String,
    },
    /// Publishes refreshed agent metadata.
    AgentUpdated {
        /// Updated agent snapshot.
        agent: Agent,
    },
    /// Publishes a full snapshot after session metadata, committed history, agent creation, or activity changes.
    /// Apply snapshots in stream order; activity changes do not advance `updated_at`.
    SessionUpdated {
        /// Updated session, including its current published busy state.
        session: Session,
    },
    /// Reports failure of an accepted asynchronous agent request.
    RequestFailed {
        /// Session containing the target agent.
        session_id: SessionId,
        /// Target agent identifier.
        agent_id: AgentId,
        /// Identifier returned when the request was accepted.
        request_id: String,
        /// Human-readable failure description.
        message: String,
    },
    /// Signals that the client must reload state after replay failure or stream lag.
    ResyncRequired,
}

/// Typed allowlist of supported global data events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobalEventKind {
    /// Full session summary upserts.
    #[default]
    SessionUpdated,
}

/// Global summary subscription selection, independent of transcript routes.
///
/// The feed carries every session so clients see sessions leave a filter too;
/// apply [`SessionFilter::matches`] to decide which updates to display.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalEventsQuery {
    /// Selected data-event kind.
    pub kind: GlobalEventKind,
}

/// Ephemeral global feed protocol. Reconnects require a new summary bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GlobalStreamMessage {
    /// Subscription established; fetch pages only after this boundary.
    Ready {
        /// Current global publication revision.
        revision: u64,
    },
    /// Full committed session summary, ordered by revision.
    SessionUpdated {
        /// Monotonic publication revision within this server lifetime.
        revision: u64,
        /// Session metadata and published busy state.
        session: Session,
    },
    /// Buffered updates were lost, or a reconnect attempted ephemeral replay.
    ResyncRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_query_spans_round_trip_through_flat_params() {
        for span in [
            EventSpan::Forward {
                after: 0,
                through: None,
            },
            EventSpan::Forward {
                after: 3,
                through: Some(9),
            },
            EventSpan::Latest,
            EventSpan::Before(4),
            EventSpan::ContextOf { through: 7 },
        ] {
            let query = EventsQuery {
                span,
                limit: 20,
                ..Default::default()
            };
            let wire = serde_json::to_value(&query).unwrap();
            assert_eq!(serde_json::from_value::<EventsQuery>(wire).unwrap(), query);
        }
        assert_eq!(
            serde_json::from_value::<EventsQuery>(json!({})).unwrap(),
            EventsQuery::default()
        );
    }

    #[test]
    fn events_query_rejects_conflicting_bounds() {
        for invalid in [
            json!({"recent": true, "after": 1}),
            json!({"recent": true, "before": 2}),
            json!({"before": 2, "through": 3}),
            json!({"from_context_start": true}),
            json!({"from_context_start": true, "through": 3, "after": 1}),
            json!({"after": 5, "through": 3}),
            json!({"unknown": 1}),
        ] {
            assert!(
                serde_json::from_value::<EventsQuery>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }
}
