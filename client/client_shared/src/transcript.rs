use psa_api::events::{EventType, ToolCall, ToolResult};
use psa_api::{
    Agent, AgentId, ClientQueryId, ClientQueryRequest, EventEnvelope, EventsPage, PendingEvent,
    Session, SessionId, StreamMessage,
};
use std::collections::{BTreeMap, HashMap, HashSet};

/// View-local follow state; do not share between duplicate transcript views.
#[derive(Debug, Clone)]
pub struct FollowState {
    pub near_bottom: bool,
    pub unseen_updates: usize,
}
impl Default for FollowState {
    fn default() -> Self {
        Self {
            near_bottom: true,
            unseen_updates: 0,
        }
    }
}
impl FollowState {
    pub fn set_near_bottom(&mut self, near_bottom: bool) {
        self.near_bottom = near_bottom;
        if near_bottom {
            self.unseen_updates = 0;
        }
    }
    /// Returns whether this view should scroll after a transcript update.
    pub fn record_activity(&mut self) -> bool {
        if self.near_bottom {
            return true;
        }
        self.unseen_updates = self.unseen_updates.saturating_add(1);
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    Committed(u64),
    Pending(String),
}
#[derive(Debug, Clone)]
pub struct TranscriptRow {
    pub id: RowId,
    pub agent_id: AgentId,
    pub kind: EventType,
    pub provisional: bool,
}
#[derive(Debug, Clone)]
pub struct ToolExchange {
    pub agent_id: AgentId,
    pub call_id: String,
    pub call: Option<ToolCall>,
    pub result: Option<ToolResult>,
    pub running: bool,
}
#[derive(Debug, Clone)]
pub struct Transcript {
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub committed: BTreeMap<u64, EventEnvelope>,
    pub pending: BTreeMap<String, PendingEvent>,
    pub queries: BTreeMap<ClientQueryId, ClientQueryRequest>,
    pub agents: HashMap<AgentId, Agent>,
    pub session: Option<Session>,
    pub request_failures: HashMap<String, String>,
    pub cursor: u64,
    pub needs_resync: bool,
    pub next_before: Option<u64>,
    pub has_older: bool,
    retired_pending: HashSet<String>,
}
impl Transcript {
    pub fn new(session_id: SessionId, agent_id: Option<AgentId>) -> Self {
        Self {
            session_id,
            agent_id,
            committed: BTreeMap::new(),
            pending: BTreeMap::new(),
            queries: BTreeMap::new(),
            agents: HashMap::new(),
            session: None,
            request_failures: HashMap::new(),
            cursor: 0,
            needs_resync: false,
            next_before: None,
            has_older: false,
            retired_pending: HashSet::new(),
        }
    }
    fn matches(&self, session: SessionId, agent: AgentId) -> bool {
        session == self.session_id && self.agent_id.is_none_or(|a| a == agent)
    }
    fn position(&self, event: &EventEnvelope) -> u64 {
        if self.agent_id.is_some() {
            event.event.id.0
        } else {
            event.position
        }
    }
    fn insert_event(&mut self, event: EventEnvelope) {
        if self.matches(event.session_id, event.agent_id) {
            let key = self.position(&event);
            self.committed.entry(key).or_insert(event);
        }
    }
    /// Call only before connecting the stream. The captured head is its replay cursor.
    pub fn bootstrap(&mut self, page: EventsPage) {
        self.cursor = page.head;
        self.pending.clear();
        self.queries.clear();
        self.retired_pending.clear();
        for p in &page.pending {
            if self.matches(p.session_id, p.agent_id) {
                self.pending.insert(p.id.clone(), p.clone());
            }
        }
        for q in &page.queries {
            if self.matches(q.session_id, q.agent_id) {
                self.queries.insert(q.id, q.clone());
            }
        }
        self.merge_history(page);
        self.needs_resync = false;
    }
    /// Older/late HTTP responses never change live cursor, queries or provisional state.
    pub fn merge_history(&mut self, page: EventsPage) {
        let oldest = page
            .events
            .iter()
            .filter(|e| self.matches(e.session_id, e.agent_id))
            .map(|e| self.position(e))
            .min();
        let current = self.committed.first_key_value().map(|(p, _)| *p);
        if current.is_none() || oldest.is_some_and(|p| current.is_none_or(|c| p <= c)) {
            self.next_before = page.next_before;
            self.has_older = page.has_older;
        }
        for event in page.events {
            self.insert_event(event);
        }
    }
    pub fn apply(&mut self, message: StreamMessage) {
        match message {
            StreamMessage::Snapshot {
                cursor,
                pending,
                queries,
            } => {
                self.cursor = self.cursor.max(cursor);
                self.pending.clear();
                self.queries.clear();
                self.retired_pending.clear();
                for p in pending {
                    if self.matches(p.session_id, p.agent_id) {
                        self.pending.insert(p.id.clone(), p);
                    }
                }
                for q in queries {
                    if self.matches(q.session_id, q.agent_id) {
                        self.queries.insert(q.id, q);
                    }
                }
                self.needs_resync = false;
            }
            StreamMessage::Upsert { pending } => {
                if self.matches(pending.session_id, pending.agent_id)
                    && !self.retired_pending.contains(&pending.id)
                    && self
                        .pending
                        .get(&pending.id)
                        .is_none_or(|p| p.revision < pending.revision)
                {
                    self.pending.insert(pending.id.clone(), pending);
                }
            }
            StreamMessage::Committed { event, pending_id } => {
                if !self.matches(event.session_id, event.agent_id) {
                    return;
                }
                if let Some(id) = pending_id {
                    self.pending.remove(&id);
                    self.retired_pending.insert(id);
                }
                self.cursor = self.cursor.max(self.position(&event));
                self.insert_event(event);
            }
            StreamMessage::Removed {
                session_id,
                agent_id,
                id,
            } => {
                if self.matches(session_id, agent_id) {
                    self.pending.remove(&id);
                    self.retired_pending.insert(id);
                }
            }
            StreamMessage::ClientQueryRequested { query } => {
                if self.matches(query.session_id, query.agent_id) {
                    self.queries.insert(query.id, query);
                }
            }
            StreamMessage::ClientQueryRemoved {
                session_id,
                agent_id,
                query_id,
            } => {
                if self.matches(session_id, agent_id) {
                    self.queries.remove(&query_id);
                }
            }
            StreamMessage::AgentUpdated { agent } => {
                if self.matches(agent.session_id, agent.id) {
                    self.agents.insert(agent.id, agent);
                }
            }
            StreamMessage::SessionUpdated { session } => {
                if session.id == self.session_id {
                    self.session = Some(session);
                }
            }
            StreamMessage::RequestFailed {
                session_id,
                agent_id,
                request_id,
                message,
            } => {
                if self.matches(session_id, agent_id) {
                    self.request_failures.insert(request_id, message);
                }
            }
            StreamMessage::ResyncRequired => self.needs_resync = true,
        }
    }
    pub fn rows(&self) -> Vec<TranscriptRow> {
        let mut rows: Vec<_> = self
            .committed
            .iter()
            .map(|(id, e)| TranscriptRow {
                id: RowId::Committed(*id),
                agent_id: e.agent_id,
                kind: e.event.kind.clone(),
                provisional: false,
            })
            .collect();
        rows.extend(self.pending.values().map(|p| TranscriptRow {
            id: RowId::Pending(p.id.clone()),
            agent_id: p.agent_id,
            kind: p.kind.clone(),
            provisional: true,
        }));
        rows
    }
    /// Bounded tail for rendering; avoids cloning the full loaded transcript.
    pub fn recent_rows(&self, limit: usize) -> Vec<TranscriptRow> {
        let committed = self.committed.iter().map(|(id, e)| TranscriptRow {
            id: RowId::Committed(*id),
            agent_id: e.agent_id,
            kind: e.event.kind.clone(),
            provisional: false,
        });
        let pending = self.pending.values().map(|p| TranscriptRow {
            id: RowId::Pending(p.id.clone()),
            agent_id: p.agent_id,
            kind: p.kind.clone(),
            provisional: true,
        });
        let mut rows: Vec<_> = committed.chain(pending).rev().take(limit).collect();
        rows.reverse();
        rows
    }
    /// Correlates by agent and call ID, including orphan results in incomplete history.
    pub fn tool_exchanges(&self) -> Vec<ToolExchange> {
        self.build_tool_exchanges(None)
    }
    /// Only clone exchanges needed by the visible rows, retaining offscreen matches.
    pub fn tool_exchanges_for(&self, call_ids: &HashSet<String>) -> Vec<ToolExchange> {
        self.build_tool_exchanges(Some(call_ids))
    }
    fn build_tool_exchanges(&self, call_ids: Option<&HashSet<String>>) -> Vec<ToolExchange> {
        let mut out: Vec<ToolExchange> = Vec::new();
        let mut indices: HashMap<(AgentId, String), usize> = HashMap::new();
        let committed = self.committed.values().map(|e| (e.agent_id, &e.event.kind));
        let pending = self.pending.values().map(|p| (p.agent_id, &p.kind));
        for (agent_id, kind) in committed.chain(pending) {
            let id = match kind {
                EventType::ToolCall(c) => &c.call_id,
                EventType::ToolResult(r) => &r.call_id,
                _ => continue,
            };
            if call_ids.is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let (call, result) = match kind {
                EventType::ToolCall(c) => (Some(c.clone()), None),
                EventType::ToolResult(r) => (None, Some(r.clone())),
                _ => unreachable!(),
            };
            let key = (agent_id, id.clone());
            let index = *indices.entry(key).or_insert_with(|| {
                out.push(ToolExchange {
                    agent_id,
                    call_id: id.clone(),
                    call: None,
                    result: None,
                    running: true,
                });
                out.len() - 1
            });
            if call.is_some() {
                out[index].call = call;
            }
            if result.is_some() {
                out[index].result = result;
                out[index].running = false;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa_api::events::{Event, EventId, Message, MessageType};
    fn pending(revision: u64) -> PendingEvent {
        PendingEvent {
            id: "p".into(),
            revision,
            session_id: SessionId(1),
            agent_id: AgentId(2),
            kind: EventType::Message(Message::text(MessageType::Agent, revision.to_string())),
        }
    }
    fn event(id: u64) -> EventEnvelope {
        EventEnvelope {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            position: id + 100,
            event: Event {
                id: EventId(id),
                source_agent_id: AgentId(2),
                timestamp: time::OffsetDateTime::UNIX_EPOCH,
                kind: pending(1).kind,
            },
        }
    }
    fn page(events: Vec<EventEnvelope>) -> EventsPage {
        EventsPage {
            events,
            pending: vec![pending(1)],
            queries: vec![],
            next_after: 0,
            next_before: Some(1),
            has_older: true,
            head: 4,
            has_more: false,
        }
    }
    #[test]
    fn provisional_commit_and_duplicate_replay() {
        let mut t = Transcript::new(SessionId(1), Some(AgentId(2)));
        t.apply(StreamMessage::Upsert {
            pending: pending(2),
        });
        t.apply(StreamMessage::Upsert {
            pending: pending(1),
        });
        assert_eq!(t.pending["p"].revision, 2);
        t.apply(StreamMessage::Committed {
            event: event(3),
            pending_id: Some("p".into()),
        });
        t.apply(StreamMessage::Committed {
            event: event(3),
            pending_id: None,
        });
        t.apply(StreamMessage::Upsert {
            pending: pending(1),
        });
        assert_eq!(t.rows().len(), 1);
        assert_eq!(t.cursor, 3);
    }
    #[test]
    fn late_history_preserves_live_state_and_cursor() {
        let mut t = Transcript::new(SessionId(1), Some(AgentId(2)));
        t.apply(StreamMessage::Upsert {
            pending: pending(3),
        });
        t.apply(StreamMessage::Committed {
            event: event(5),
            pending_id: None,
        });
        t.merge_history(page(vec![event(2), event(1)]));
        assert_eq!(t.cursor, 5);
        assert_eq!(t.pending["p"].revision, 3);
        assert_eq!(
            t.committed.keys().copied().collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
    }
    #[test]
    fn session_and_agent_cursors_differ() {
        let mut t = Transcript::new(SessionId(1), None);
        t.apply(StreamMessage::Committed {
            event: event(1),
            pending_id: None,
        });
        assert_eq!(t.cursor, 101);
    }
    #[test]
    fn snapshot_authoritatively_removes_questions_and_pending() {
        let mut t = Transcript::new(SessionId(1), None);
        t.apply(StreamMessage::Upsert {
            pending: pending(1),
        });
        t.apply(StreamMessage::Snapshot {
            cursor: 3,
            pending: vec![],
            queries: vec![],
        });
        assert!(t.pending.is_empty());
        t.apply(StreamMessage::ResyncRequired);
        assert!(t.needs_resync);
    }
}
