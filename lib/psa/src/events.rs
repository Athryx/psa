//! Event range queries over an agent's or a session's transcript.
//!
//! A range has a start and an end bound. Cursors are agent event IDs for agent
//! ranges and session positions for session ranges.
use crate::{
    Error, Result,
    api::{
        self, Agent, AgentId, ClientQueryId, ClientQueryRequest, EventEnvelope, EventSpan,
        EventsPage, EventsQuery, PendingEvent, PendingEventId, RequestId, Session, SessionId,
        StreamMessage,
    },
};
use futures_util::{Stream, StreamExt, stream};
use std::pin::Pin;

/// Page size for range reads.
const PAGE_LIMIT: u32 = 1000;
/// Largest page the server accepts.
const MAX_PAGE_LIMIT: u32 = 10_000;

/// Where an event range starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStart {
    /// The first event of the agent's lineage or the session.
    Beginning,
    /// The start of the agent's current model context. Agent ranges only.
    CurrentContext,
    /// The start of the model context containing this event. Agent ranges only.
    ContextOf(u64),
    /// Just after this cursor.
    After(u64),
    /// At this cursor, inclusive.
    At(u64),
    /// The current end: only events committed from now on.
    Head,
    /// The newest `n` events up to the range end; the newest `n` results with
    /// [`EventRange::results_only`].
    Last(u32),
}

/// Where an event range ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventEnd {
    /// The end of the transcript when the range is read.
    Head,
    /// This cursor, inclusive.
    Through(u64),
    /// Keep following new events. Read only with `stream`.
    Follow,
}

/// A span of transcript events and how to filter them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRange {
    /// First bound.
    pub start: EventStart,
    /// Last bound.
    pub end: EventEnd,
    /// Return only durable result events.
    pub results_only: bool,
    /// Include provisional events. Only streams carry them.
    pub include_pending: bool,
}

impl Default for EventRange {
    fn default() -> Self {
        Self::all()
    }
}

impl EventRange {
    /// Every event up to the current end.
    pub fn all() -> Self {
        Self {
            start: EventStart::Beginning,
            end: EventEnd::Head,
            results_only: false,
            include_pending: false,
        }
    }

    /// The agent's current model context.
    pub fn current_context() -> Self {
        Self::all().starting(EventStart::CurrentContext)
    }

    /// The newest `n` events.
    pub fn last(n: u32) -> Self {
        Self::all().starting(EventStart::Last(n))
    }

    /// Only events committed from now on, followed indefinitely.
    pub fn follow() -> Self {
        Self::all()
            .starting(EventStart::Head)
            .ending(EventEnd::Follow)
    }

    /// Replace the start bound.
    pub fn starting(mut self, start: EventStart) -> Self {
        self.start = start;
        self
    }

    /// Replace the end bound.
    pub fn ending(mut self, end: EventEnd) -> Self {
        self.end = end;
        self
    }

    /// Keep following new events after the range's history.
    pub fn following(self) -> Self {
        self.ending(EventEnd::Follow)
    }

    /// Return only durable result events.
    pub fn results_only(mut self) -> Self {
        self.results_only = true;
        self
    }

    /// Include provisional events in streams.
    pub fn with_pending(mut self) -> Self {
        self.include_pending = true;
        self
    }
}

/// A change seen while streaming an event range.
#[derive(Debug, Clone)]
pub enum EventUpdate {
    /// A committed transcript event.
    Committed(EventEnvelope),
    /// Replay of the range's history finished; later updates are live. Replaces
    /// all provisional events and outstanding queries; sent again after a reconnect.
    Live {
        /// Current provisional events, when requested.
        pending: Vec<PendingEvent>,
        /// Outstanding client queries.
        queries: Vec<ClientQueryRequest>,
    },
    /// A provisional event was created or replaced.
    Pending(PendingEvent),
    /// A provisional event was dropped without being committed.
    PendingRemoved {
        /// Agent that published it.
        agent_id: AgentId,
        /// Provisional event identifier.
        id: PendingEventId,
    },
    /// An agent is waiting for a client answer.
    QueryRequested(ClientQueryRequest),
    /// A client query is no longer outstanding.
    QueryRemoved {
        /// Agent that asked.
        agent_id: AgentId,
        /// Query identifier.
        query_id: ClientQueryId,
    },
    /// New agent metadata, state, or usage.
    Agent(Agent),
    /// New session metadata or busy state.
    Session(Session),
    /// An accepted request failed.
    RequestFailed {
        /// Target agent.
        agent_id: AgentId,
        /// Identifier returned when the request was accepted.
        request_id: RequestId,
        /// Failure description.
        message: String,
    },
}

/// Lazily fetched pages of committed events.
pub type EventPages = Pin<Box<dyn Stream<Item = Result<Vec<EventEnvelope>>> + Send>>;
/// Committed events followed by live updates. Reconnects after the server asks
/// for a resync or a following stream ends; ends after a transport error.
pub type EventFeed = Pin<Box<dyn Stream<Item = Result<EventUpdate>> + Send>>;

/// The transcript an event range reads: a session, or one agent in it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scope {
    pub session: SessionId,
    pub agent: Option<AgentId>,
}

impl Scope {
    fn cursor_of(&self, event: &EventEnvelope) -> u64 {
        match self.agent {
            Some(_) => event.event.id.0,
            None => event.position,
        }
    }

    async fn page(
        &self,
        client: &api::Client,
        span: EventSpan,
        limit: u32,
        results_only: bool,
    ) -> Result<EventsPage> {
        let query = EventsQuery {
            span,
            limit,
            include_pending: false,
            results_only,
        };
        Ok(client.list_events(self.session, self.agent, &query).await?)
    }

    /// Cursor of the newest committed event, or 0 when there is none.
    pub(crate) async fn head(&self, client: &api::Client) -> Result<u64> {
        Ok(self.page(client, EventSpan::Latest, 1, false).await?.head)
    }

    /// Read every committed event in a finite range.
    pub(crate) async fn events(
        &self,
        client: &api::Client,
        range: EventRange,
    ) -> Result<Vec<EventEnvelope>> {
        let mut pages = self.event_pages(client, range).await?;
        let mut events = Vec::new();
        while let Some(page) = pages.next().await {
            events.extend(page?);
        }
        Ok(events)
    }

    /// Resolve a finite range, then fetch its pages lazily.
    pub(crate) async fn event_pages(
        &self,
        client: &api::Client,
        range: EventRange,
    ) -> Result<EventPages> {
        let through = match range.end {
            EventEnd::Head => self.head(client).await?,
            EventEnd::Through(cursor) => cursor,
            EventEnd::Follow => {
                return Err(Error::InvalidRange(
                    "a following range is read with stream()",
                ));
            }
        };
        let after = self.resolve_after(client, &range, Some(through)).await?;

        let scope = *self;
        let client = client.clone();
        let pages = stream::try_unfold(Some(after), move |after| {
            let client = client.clone();
            async move {
                let Some(after) = after.filter(|after| *after < through) else {
                    return Ok(None);
                };
                let span = EventSpan::Forward {
                    after,
                    through: Some(through),
                };
                let page = scope
                    .page(&client, span, PAGE_LIMIT, range.results_only)
                    .await?;
                let next = page.has_more.then_some(page.next_after);
                Ok(Some((page.events, next)))
            }
        });
        Ok(Box::pin(pages))
    }

    /// Resolve a range and open its stream before returning, so no update after
    /// this call is missed.
    pub(crate) async fn stream(
        &self,
        client: &api::Client,
        range: EventRange,
    ) -> Result<EventFeed> {
        let through = match range.end {
            EventEnd::Head => Some(self.head(client).await?),
            EventEnd::Through(cursor) => Some(cursor),
            EventEnd::Follow => None,
        };
        let after = self.resolve_after(client, &range, through).await?;

        let mut feed = Feed {
            client: client.clone(),
            scope: *self,
            range,
            after,
            through,
            stream: None,
            done: false,
        };
        feed.connect().await?;
        Ok(Box::pin(stream::unfold(feed, |mut feed| async move {
            let update = feed.next_update().await?;
            Some((update, feed))
        })))
    }

    /// The exclusive cursor a range's events follow.
    async fn resolve_after(
        &self,
        client: &api::Client,
        range: &EventRange,
        through: Option<u64>,
    ) -> Result<u64> {
        match range.start {
            EventStart::Beginning => Ok(0),
            EventStart::After(cursor) => Ok(cursor),
            EventStart::At(cursor) => Ok(cursor.saturating_sub(1)),
            EventStart::Head => self.head(client).await,
            EventStart::CurrentContext => {
                let head = self.head(client).await?;
                self.context_start(client, head).await
            }
            EventStart::ContextOf(cursor) => self.context_start(client, cursor).await,
            EventStart::Last(count) => {
                self.last_start(client, count, through, range.results_only)
                    .await
            }
        }
    }

    /// The exclusive cursor before the model context containing `cursor`.
    async fn context_start(&self, client: &api::Client, cursor: u64) -> Result<u64> {
        if self.agent.is_none() {
            return Err(Error::InvalidRange(
                "model contexts belong to agents, not sessions",
            ));
        }
        if cursor == 0 {
            return Ok(0);
        }

        let page = self
            .page(client, EventSpan::ContextOf { through: cursor }, 1, false)
            .await?;
        Ok(page
            .events
            .first()
            .map_or(0, |event| self.cursor_of(event) - 1))
    }

    /// The exclusive cursor before the newest `count` events up to `through`.
    async fn last_start(
        &self,
        client: &api::Client,
        count: u32,
        through: Option<u64>,
        results_only: bool,
    ) -> Result<u64> {
        if count == 0 {
            return match through {
                Some(through) => Ok(through),
                None => self.head(client).await,
            };
        }

        let mut span = match through {
            Some(through) => EventSpan::Before(through + 1),
            None => EventSpan::Latest,
        };
        let mut remaining = count;
        let mut after = 0;
        loop {
            let limit = remaining.min(MAX_PAGE_LIMIT);
            let page = self.page(client, span, limit, results_only).await?;
            if let Some(first) = page.events.first() {
                after = self.cursor_of(first) - 1;
            }

            remaining = remaining.saturating_sub(page.events.len() as u32);
            let Some(before) = page.next_before.filter(|_| remaining > 0) else {
                return Ok(after);
            };
            span = EventSpan::Before(before);
        }
    }
}

/// Stream state for [`Scope::stream`], reconnecting from the newest cursor.
struct Feed {
    client: api::Client,
    scope: Scope,
    range: EventRange,
    after: u64,
    through: Option<u64>,
    stream: Option<api::EventStream>,
    done: bool,
}

impl Feed {
    async fn connect(&mut self) -> Result<()> {
        let query = EventsQuery {
            span: EventSpan::Forward {
                after: self.after,
                through: self.through,
            },
            limit: PAGE_LIMIT,
            include_pending: self.range.include_pending,
            results_only: self.range.results_only,
        };
        let stream = self
            .client
            .stream_events(self.scope.session, self.scope.agent, &query, None)
            .await?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn next_update(&mut self) -> Option<Result<EventUpdate>> {
        loop {
            if self.done {
                return None;
            }
            let Some(stream) = self.stream.as_mut() else {
                if let Err(error) = self.connect().await {
                    self.done = true;
                    return Some(Err(error));
                }
                continue;
            };

            match stream.next().await {
                None => {
                    // A bounded stream ends after its range; a following one reconnects.
                    self.stream = None;
                    self.done = self.through.is_some();
                }
                Some(Err(error)) => {
                    self.done = true;
                    return Some(Err(error.into()));
                }
                Some(Ok(event)) => {
                    if let Some(update) = self.apply_message(event.message) {
                        return Some(Ok(update));
                    }
                }
            }
        }
    }

    fn apply_message(&mut self, message: StreamMessage) -> Option<EventUpdate> {
        let update = match message {
            StreamMessage::Committed { event, .. } => {
                self.after = self.after.max(self.scope.cursor_of(&event));
                EventUpdate::Committed(event)
            }
            StreamMessage::Snapshot {
                cursor,
                pending,
                queries,
            } => {
                self.after = self.after.max(cursor);
                EventUpdate::Live { pending, queries }
            }
            StreamMessage::ResyncRequired => {
                self.stream = None;
                return None;
            }
            StreamMessage::Upsert { pending } => EventUpdate::Pending(pending),
            StreamMessage::Removed { agent_id, id, .. } => {
                EventUpdate::PendingRemoved { agent_id, id }
            }
            StreamMessage::ClientQueryRequested { query } => EventUpdate::QueryRequested(query),
            StreamMessage::ClientQueryRemoved {
                agent_id, query_id, ..
            } => EventUpdate::QueryRemoved { agent_id, query_id },
            StreamMessage::AgentUpdated { agent } => EventUpdate::Agent(agent),
            StreamMessage::SessionUpdated { session } => EventUpdate::Session(session),
            StreamMessage::RequestFailed {
                agent_id,
                request_id,
                message,
                ..
            } => EventUpdate::RequestFailed {
                agent_id,
                request_id,
                message,
            },
        };
        Some(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_builders_set_bounds_and_filters() {
        assert_eq!(
            EventRange::follow(),
            EventRange {
                start: EventStart::Head,
                end: EventEnd::Follow,
                results_only: false,
                include_pending: false,
            }
        );

        let range = EventRange::last(5)
            .ending(EventEnd::Through(40))
            .results_only()
            .with_pending();
        assert_eq!(range.start, EventStart::Last(5));
        assert_eq!(range.end, EventEnd::Through(40));
        assert!(range.results_only && range.include_pending);

        let context = EventRange::current_context().following();
        assert_eq!(context.start, EventStart::CurrentContext);
        assert_eq!(context.end, EventEnd::Follow);
    }
}
