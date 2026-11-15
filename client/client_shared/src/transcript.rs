use psa_api::events::{EventType, MessageType, ToolCall, ToolResult};
use psa_api::{
    Agent, AgentId, AgentStatusSnapshot, ClientQueryId, ClientQueryRequest, EventEnvelope,
    EventsPage, PendingEvent, Session, SessionId, StreamMessage,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Deref,
    sync::{Arc, OnceLock},
};

/// Copy-on-write ordered payload storage. Snapshots share both the index and
/// individual events; replacing one streaming event never copies old payloads.
#[derive(Debug, Clone)]
pub struct PayloadMap<K, V> {
    entries: Arc<BTreeMap<K, Arc<V>>>,
    keys: Arc<Vec<K>>,
}

impl<K, V> Default for PayloadMap<K, V> {
    fn default() -> Self {
        Self {
            entries: Arc::default(),
            keys: Arc::default(),
        }
    }
}

impl<K, V> Deref for PayloadMap<K, V> {
    type Target = BTreeMap<K, Arc<V>>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl<K: Ord + Clone, V> PayloadMap<K, V> {
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<Arc<V>> {
        if let Err(index) = self.keys.binary_search(&key) {
            Arc::make_mut(&mut self.keys).insert(index, key.clone());
        }
        Arc::make_mut(&mut self.entries).insert(key, Arc::new(value))
    }

    pub(crate) fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<Arc<V>>
    where
        K: std::borrow::Borrow<Q>,
    {
        let index = self
            .keys
            .binary_search_by(|entry| entry.borrow().cmp(key))
            .ok()?;
        Arc::make_mut(&mut self.keys).remove(index);
        Arc::make_mut(&mut self.entries).remove(key)
    }

    pub(crate) fn clear(&mut self) {
        self.entries = Arc::default();
        self.keys = Arc::default();
    }

    fn key_at(&self, index: usize) -> Option<&K> {
        self.keys.get(index)
    }

    fn position(&self, key: &K) -> Option<usize> {
        self.keys.binary_search(key).ok()
    }
}

/// A cheap, identity-comparable row snapshot, including its revision and routing.
#[derive(Debug, Clone)]
pub enum RowPayload {
    Committed(u64, Arc<EventEnvelope>),
    Pending(Arc<PendingEvent>),
}

impl PartialEq for RowPayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Committed(a, left), Self::Committed(b, right)) => {
                a == b && Arc::ptr_eq(left, right)
            }
            (Self::Pending(left), Self::Pending(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}
impl Eq for RowPayload {}

/// A successful inference has nothing to report; it only bounds a model turn.
pub fn is_successful_inference(end: &psa_api::events::InferenceResultEnd) -> bool {
    matches!(
        end.outcome,
        llm_api::api::Outcome::Completed { .. } | llm_api::api::Outcome::InProgress
    )
}

fn committed_event_id(payload: &RowPayload) -> Option<u64> {
    match payload {
        RowPayload::Committed(_, envelope) => Some(envelope.event.id.0),
        RowPayload::Pending(_) => None,
    }
}

impl RowPayload {
    pub fn id(&self) -> RowId {
        match self {
            Self::Committed(id, _) => RowId::Committed(*id),
            Self::Pending(event) => RowId::Pending(event.id.clone()),
        }
    }

    pub fn kind(&self) -> &EventType {
        match self {
            Self::Committed(_, event) => &event.event.kind,
            Self::Pending(event) => &event.kind,
        }
    }

    pub fn agent_id(&self) -> AgentId {
        match self {
            Self::Committed(_, event) => event.agent_id,
            Self::Pending(event) => event.agent_id,
        }
    }

    pub fn is_provisional(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

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

/// A model turn's ordinal, as far as loaded history can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnNumber {
    pub number: usize,
    /// False when unloaded older history may hold earlier turns.
    pub exact: bool,
}

const DISPLAY_CATEGORIES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub committed: PayloadMap<u64, EventEnvelope>,
    pub pending: PayloadMap<String, PendingEvent>,
    pub queries: BTreeMap<ClientQueryId, ClientQueryRequest>,
    pub agents: HashMap<AgentId, Agent>,
    pub session: Option<Session>,
    /// Server projection at a committed agent event boundary.
    pub status_snapshot: Option<AgentStatusSnapshot>,
    pub request_failures: HashMap<String, String>,
    pub cursor: u64,
    pub needs_resync: bool,
    pub next_before: Option<u64>,
    pub has_older: bool,
    retired_pending: Arc<HashSet<String>>,
    tool_results: Arc<HashMap<(AgentId, String), Arc<BTreeMap<RowId, RowPayload>>>>,
    display_index: [Arc<Vec<RowId>>; DISPLAY_CATEGORIES],
    human_messages: Arc<Vec<RowId>>,
    inference_starts: Arc<Vec<RowId>>,
    inference_ends: Arc<Vec<RowId>>,
    context_resets: Arc<Vec<RowId>>,
    fallback_status: Arc<OnceLock<AgentStatusSnapshot>>,
}
impl Transcript {
    pub fn new(session_id: SessionId, agent_id: Option<AgentId>) -> Self {
        Self {
            session_id,
            agent_id,
            committed: PayloadMap::default(),
            pending: PayloadMap::default(),
            queries: BTreeMap::new(),
            agents: HashMap::new(),
            session: None,
            status_snapshot: None,
            request_failures: HashMap::new(),
            cursor: 0,
            needs_resync: false,
            next_before: None,
            has_older: false,
            retired_pending: Arc::default(),
            tool_results: Arc::default(),
            display_index: Default::default(),
            human_messages: Arc::default(),
            inference_starts: Arc::default(),
            inference_ends: Arc::default(),
            context_resets: Arc::default(),
            fallback_status: Arc::default(),
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
            if let Some(status) = &mut self.status_snapshot {
                status.apply_event(&event.event);
            }
            if self.committed.contains_key(&key) {
                return;
            }
            self.committed.insert(key, event);
            self.fallback_status = Arc::default();
            let payload = RowPayload::Committed(key, self.committed[&key].clone());
            self.index_display_row(&payload);
            self.index_human_message(&payload);
            self.index_context_marker(&payload);
            self.index_tool_result(payload);
        }
    }
    /// Cache the ordered fallback for this committed set. Pending-only clones
    /// share its initialization; late history invalidates it just like live commits.
    pub(crate) fn fallback_status_snapshot(&self) -> &AgentStatusSnapshot {
        self.fallback_status.get_or_init(|| {
            let mut status = AgentStatusSnapshot::default();
            let Some(agent_id) = self.agent_id else {
                return status;
            };
            for envelope in self.committed.values() {
                if envelope.session_id == self.session_id && envelope.agent_id == agent_id {
                    status.apply_event(&envelope.event);
                }
            }
            status
        })
    }

    fn display_category(kind: &EventType) -> Option<usize> {
        match kind {
            EventType::Message(_)
            | EventType::Log(_)
            | EventType::Command(_)
            | EventType::DecisionCall(_)
            | EventType::ToolCall(_)
            | EventType::ToolResult(_)
            | EventType::Compaction(_)
            | EventType::ProviderItem(_) => Some(0),
            EventType::Thinking(thinking)
                if thinking
                    .contents
                    .summary
                    .iter()
                    .any(|text| !text.is_empty()) =>
            {
                Some(1)
            }
            EventType::Result(psa_api::AgentResult::Text(_)) => Some(2),
            EventType::Result(_) => Some(0),
            EventType::StatusChange(status)
                if status.new_status == psa_api::AgentState::Interrupted =>
            {
                Some(0)
            }
            EventType::StatusChange(_) => Some(3),
            // Each turn opens with an optional divider at its start, between
            // what it was sent and what it produced. A failed or cancelled
            // end is always reported; a successful one has nothing to show.
            EventType::InferenceResultStart(_) => Some(4),
            EventType::InferenceResultEnd(end) if !is_successful_inference(end) => Some(0),
            _ => None,
        }
    }

    fn index_display_row(&mut self, payload: &RowPayload) {
        if let Some(category) = Self::display_category(payload.kind()) {
            let id = payload.id();
            if let Err(index) = self.display_index[category].binary_search(&id) {
                Arc::make_mut(&mut self.display_index[category]).insert(index, id);
            }
        }
    }

    fn remove_display_row(&mut self, id: &RowId) {
        for ids in &mut self.display_index {
            if let Ok(index) = ids.binary_search(id) {
                Arc::make_mut(ids).remove(index);
                break;
            }
        }
    }

    fn is_human_message(kind: &EventType) -> bool {
        matches!(kind, EventType::Message(message) if message.message_type == MessageType::Human)
    }

    fn index_human_message(&mut self, payload: &RowPayload) {
        if !Self::is_human_message(payload.kind()) {
            return;
        }
        let id = payload.id();
        if let Err(index) = self.human_messages.binary_search(&id) {
            Arc::make_mut(&mut self.human_messages).insert(index, id);
        }
    }

    fn remove_human_message(&mut self, id: &RowId) {
        if let Ok(index) = self.human_messages.binary_search(id) {
            Arc::make_mut(&mut self.human_messages).remove(index);
        }
    }

    /// Index the events that delimit model contexts: inference starts and
    /// ends, and the markers that open a new context.
    fn index_context_marker(&mut self, payload: &RowPayload) {
        let markers = match payload.kind() {
            EventType::InferenceResultStart(_) => &mut self.inference_starts,
            EventType::InferenceResultEnd(_) => &mut self.inference_ends,
            EventType::NewContext => &mut self.context_resets,
            _ => return,
        };
        let id = payload.id();
        if let Err(index) = markers.binary_search(&id) {
            Arc::make_mut(markers).insert(index, id);
        }
    }

    /// The first committed marker of an agent strictly after a row.
    fn next_marker<'a>(
        &'a self,
        markers: &'a [RowId],
        after: &RowId,
        agent_id: AgentId,
    ) -> Option<(&'a RowId, &'a EventEnvelope)> {
        let first = markers.partition_point(|marker| marker <= after);
        markers[first..].iter().find_map(|marker| {
            let RowId::Committed(key) = marker else {
                return None;
            };
            let envelope = self.committed.get(key)?;
            (envelope.agent_id == agent_id).then_some((marker, &**envelope))
        })
    }

    /// Number a model turn by its inference start, counting from the user's
    /// preceding message. Without one, turns count from the start of history.
    ///
    /// Only loaded history is counted: while older history exists and no user
    /// message precedes the turn, the number is a lower bound.
    pub fn turn_number(&self, id: &RowId) -> Option<TurnNumber> {
        let position = self.inference_starts.binary_search(id).ok()?;
        let preceding_messages = self.human_messages.partition_point(|message| message < id);
        let first_counted = match preceding_messages.checked_sub(1) {
            Some(index) => {
                let message = &self.human_messages[index];
                self.inference_starts
                    .partition_point(|start| start < message)
            }
            None => 0,
        };
        Some(TurnNumber {
            number: position - first_counted + 1,
            exact: preceding_messages > 0 || !self.has_older,
        })
    }

    /// The most recent inference end in loaded history.
    pub fn latest_turn(&self) -> Option<RowId> {
        self.inference_ends.last().cloned()
    }

    /// The end of the inference a start row opened, once it has ended.
    pub fn inference_end(&self, start: &RowId) -> Option<RowPayload> {
        let RowId::Committed(key) = start else {
            return None;
        };
        let envelope = self.committed.get(key)?;
        let EventType::InferenceResultStart(start_marker) = &envelope.event.kind else {
            return None;
        };
        let (end_row, end) = self.next_marker(&self.inference_ends, start, envelope.agent_id)?;
        match &end.event.kind {
            EventType::InferenceResultEnd(end_marker) if end_marker.id == start_marker.id => {
                self.row_payload(end_row)
            }
            _ => None,
        }
    }

    /// The inference end naming the first context a row was sent in, so its
    /// context view always contains the row. A turn's context is everything
    /// before its start, so a turn's own output belongs to the next turn. The
    /// start and end of an inference stand for their own turn.
    ///
    /// `None` until a later inference of the row's agent has ended, or when a
    /// new context opened before one started, as the row was never sent.
    pub fn context_turn(&self, id: &RowId) -> Option<u64> {
        let payload = self.row_payload(id)?;
        let agent_id = payload.agent_id();
        let start_row = match payload.kind() {
            EventType::InferenceResultEnd(_) => return committed_event_id(&payload),
            EventType::InferenceResultStart(_) => id,
            _ => {
                let (start_row, _) = self.next_marker(&self.inference_starts, id, agent_id)?;
                let reset = self.next_marker(&self.context_resets, id, agent_id);
                if reset.is_some_and(|(reset_row, _)| reset_row < start_row) {
                    return None;
                }
                start_row
            }
        };
        committed_event_id(&self.inference_end(start_row)?)
    }

    /// Loaded human messages in transcript order, for navigation between turns.
    pub fn human_message_ids(&self) -> &[RowId] {
        &self.human_messages
    }

    pub fn row_payload(&self, id: &RowId) -> Option<RowPayload> {
        match id {
            RowId::Committed(id) => self
                .committed
                .get(id)
                .map(|event| RowPayload::Committed(*id, event.clone())),
            RowId::Pending(id) => self
                .pending
                .get(id)
                .map(|event| RowPayload::Pending(event.clone())),
        }
    }

    fn enabled_categories(preferences: &crate::UiPreferences) -> [bool; DISPLAY_CATEGORIES] {
        [
            true,
            preferences.reasoning_display != crate::ReasoningDisplay::Hidden,
            !preferences.hide_text_recorded_results,
            !preferences.hide_agent_statuses,
            preferences.turn_dividers != crate::TurnDividerDisplay::Hidden,
        ]
    }

    pub fn display_count(&self, preferences: &crate::UiPreferences) -> usize {
        Self::enabled_categories(preferences)
            .iter()
            .zip(&self.display_index)
            .filter(|(enabled, _)| **enabled)
            .map(|(_, ids)| ids.len())
            .sum()
    }

    fn display_rank(&self, id: &RowId, preferences: &crate::UiPreferences) -> usize {
        Self::enabled_categories(preferences)
            .iter()
            .zip(&self.display_index)
            .filter(|(enabled, _)| **enabled)
            .map(|(_, ids)| ids.partition_point(|candidate| candidate < id))
            .sum()
    }

    pub fn display_position(
        &self,
        id: &RowId,
        preferences: &crate::UiPreferences,
    ) -> Option<usize> {
        let present = Self::enabled_categories(preferences)
            .iter()
            .zip(&self.display_index)
            .any(|(enabled, ids)| *enabled && ids.binary_search(id).is_ok());
        present.then(|| self.display_rank(id, preferences))
    }

    pub fn display_id_at(&self, index: usize, preferences: &crate::UiPreferences) -> Option<RowId> {
        if index >= self.display_count(preferences) {
            return None;
        }
        let mut low = 0;
        let mut high = self.row_count();
        // Binary-search the raw ordered rank, summing the four category ranks.
        // No loaded-history scan is needed for either tail or distant windows.
        while low < high {
            let middle = low + (high - low) / 2;
            let id = self.row_id_at(middle)?;
            let through = Self::enabled_categories(preferences)
                .iter()
                .zip(&self.display_index)
                .filter(|(enabled, _)| **enabled)
                .map(|(_, ids)| ids.partition_point(|candidate| candidate <= &id))
                .sum::<usize>();
            if through <= index {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        self.row_id_at(low)
    }

    pub fn display_window(
        &self,
        start: usize,
        limit: usize,
        preferences: &crate::UiPreferences,
    ) -> Vec<RowPayload> {
        let Some(first) = self.display_id_at(start, preferences) else {
            return Vec::new();
        };
        let enabled = Self::enabled_categories(preferences);
        let mut positions = self
            .display_index
            .each_ref()
            .map(|ids| ids.partition_point(|id| id < &first));
        let mut rows =
            Vec::with_capacity(limit.min(self.display_count(preferences).saturating_sub(start)));
        while rows.len() < limit {
            let next = (0..DISPLAY_CATEGORIES)
                .filter(|category| enabled[*category])
                .filter_map(|category| {
                    self.display_index[category]
                        .get(positions[category])
                        .map(|id| (category, id))
                })
                .min_by(|(_, left), (_, right)| left.cmp(right));
            let Some((category, id)) = next else {
                break;
            };
            let payload = match id {
                RowId::Committed(id) => RowPayload::Committed(*id, self.committed[id].clone()),
                RowId::Pending(id) => RowPayload::Pending(self.pending[id].clone()),
            };
            rows.push(payload);
            positions[category] += 1;
        }
        rows
    }

    fn index_tool_result(&mut self, payload: RowPayload) {
        if let EventType::ToolResult(result) = payload.kind() {
            let key = (payload.agent_id(), result.call_id.clone());
            let entries = Arc::make_mut(&mut self.tool_results)
                .entry(key)
                .or_default();
            Arc::make_mut(entries).insert(payload.id(), payload);
        }
    }

    fn remove_tool_result(&mut self, payload: &RowPayload) {
        if let EventType::ToolResult(result) = payload.kind() {
            let key = (payload.agent_id(), result.call_id.clone());
            let results = Arc::make_mut(&mut self.tool_results);
            if let Some(entries) = results.get_mut(&key) {
                Arc::make_mut(entries).remove(&payload.id());
                if entries.is_empty() {
                    results.remove(&key);
                }
            }
        }
    }

    fn insert_pending(&mut self, pending: PendingEvent) {
        let id = pending.id.clone();
        let previous = self.pending.get(&id).cloned();
        let old_category = previous
            .as_ref()
            .and_then(|event| Self::display_category(&event.kind));
        let new_category = Self::display_category(&pending.kind);
        if old_category != new_category {
            self.remove_display_row(&RowId::Pending(id.clone()));
        }
        if let Some(previous) = previous {
            if Self::is_human_message(&previous.kind) && !Self::is_human_message(&pending.kind) {
                self.remove_human_message(&RowId::Pending(id.clone()));
            }
            self.remove_tool_result(&RowPayload::Pending(previous));
        }
        self.pending.insert(id.clone(), pending);
        let payload = RowPayload::Pending(self.pending[&id].clone());
        self.index_display_row(&payload);
        self.index_human_message(&payload);
        self.index_tool_result(payload);
    }

    fn clear_pending(&mut self) {
        let previous: Vec<_> = self
            .pending
            .values()
            .cloned()
            .map(RowPayload::Pending)
            .collect();
        for payload in previous {
            self.remove_display_row(&payload.id());
            self.remove_human_message(&payload.id());
            self.remove_tool_result(&payload);
        }
        self.pending.clear();
    }

    fn remove_pending(&mut self, id: &str) {
        self.remove_display_row(&RowId::Pending(id.to_owned()));
        self.remove_human_message(&RowId::Pending(id.to_owned()));
        if let Some(previous) = self.pending.remove(id) {
            self.remove_tool_result(&RowPayload::Pending(previous));
        }
    }

    /// Indexed correlation includes offscreen results and is scoped by agent.
    pub fn tool_result(&self, agent_id: AgentId, call_id: &str) -> Option<RowPayload> {
        self.tool_results
            .get(&(agent_id, call_id.to_owned()))
            .and_then(|entries| entries.last_key_value().map(|(_, payload)| payload.clone()))
    }

    pub fn row_count(&self) -> usize {
        self.committed.len() + self.pending.len()
    }

    pub fn row_id_at(&self, index: usize) -> Option<RowId> {
        if index < self.committed.len() {
            self.committed.key_at(index).copied().map(RowId::Committed)
        } else {
            self.pending
                .key_at(index - self.committed.len())
                .cloned()
                .map(RowId::Pending)
        }
    }

    pub fn row_position(&self, id: &RowId) -> Option<usize> {
        match id {
            RowId::Committed(id) => self.committed.position(id),
            RowId::Pending(id) => self
                .pending
                .position(id)
                .map(|index| self.committed.len() + index),
        }
    }

    /// Clone only shared handles for a bounded, indexed viewport.
    pub fn row_window(&self, start: usize, limit: usize) -> Vec<RowPayload> {
        (start..start.saturating_add(limit).min(self.row_count()))
            .filter_map(|index| {
                if index < self.committed.len() {
                    let id = *self.committed.key_at(index)?;
                    Some(RowPayload::Committed(id, self.committed.get(&id)?.clone()))
                } else {
                    let id = self.pending.key_at(index - self.committed.len())?;
                    Some(RowPayload::Pending(self.pending.get(id)?.clone()))
                }
            })
            .collect()
    }

    /// Call only before connecting the stream. The captured head is its replay cursor.
    pub fn bootstrap(&mut self, mut page: EventsPage) {
        self.cursor = page.head;
        self.status_snapshot = None;
        self.clear_pending();
        self.queries.clear();
        self.retired_pending = Arc::default();
        for pending in std::mem::take(&mut page.pending) {
            if self.matches(pending.session_id, pending.agent_id) {
                self.insert_pending(pending);
            }
        }
        for query in std::mem::take(&mut page.queries) {
            if self.matches(query.session_id, query.agent_id) {
                self.queries.insert(query.id, query);
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
                self.clear_pending();
                self.queries.clear();
                self.retired_pending = Arc::default();
                for pending in pending {
                    if self.matches(pending.session_id, pending.agent_id) {
                        self.insert_pending(pending);
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
                    self.insert_pending(pending);
                }
            }
            StreamMessage::Committed { event, pending_id } => {
                if !self.matches(event.session_id, event.agent_id) {
                    return;
                }
                if let Some(id) = pending_id {
                    self.remove_pending(&id);
                    Arc::make_mut(&mut self.retired_pending).insert(id);
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
                    self.remove_pending(&id);
                    Arc::make_mut(&mut self.retired_pending).insert(id);
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
    use psa_api::events::{Event, EventId, Message};
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
    fn snapshots_share_unchanged_payloads_and_rank_indexes() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        transcript.apply(StreamMessage::Committed {
            event: event(1),
            pending_id: None,
        });
        transcript.apply(StreamMessage::Upsert {
            pending: pending(1),
        });
        let snapshot = transcript.clone();
        let rows = snapshot.row_window(0, 10);
        transcript.apply(StreamMessage::Upsert {
            pending: pending(2),
        });

        assert!(Arc::ptr_eq(
            &snapshot.committed.entries,
            &transcript.committed.entries
        ));
        assert!(Arc::ptr_eq(
            &snapshot.pending.keys,
            &transcript.pending.keys
        ));
        assert!(Arc::ptr_eq(
            &snapshot.display_index[0],
            &transcript.display_index[0]
        ));
        assert!(!Arc::ptr_eq(
            &snapshot.pending.entries,
            &transcript.pending.entries
        ));
        let updated = transcript.row_window(0, 10);
        assert_eq!(rows[0], updated[0]);
        assert_ne!(rows[1], updated[1]);
        assert_eq!(snapshot.pending["p"].revision, 1);
        assert_eq!(transcript.pending["p"].revision, 2);
    }

    #[test]
    fn display_rank_merges_categories_and_skips_arbitrarily_long_hidden_tails() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        for id in 1..=1000 {
            let mut next = event(id);
            if id > 10 {
                next.event.kind = EventType::StatusChange(psa_api::events::StatusChange {
                    new_status: psa_api::AgentState::Idle,
                    reason: None,
                });
            }
            transcript.apply(StreamMessage::Committed {
                event: next,
                pending_id: None,
            });
        }
        let mut preferences = crate::UiPreferences::default();
        preferences.hide_agent_statuses = true;
        assert_eq!(transcript.display_count(&preferences), 10);
        let rows = transcript.display_window(0, 150, &preferences);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows.last().unwrap().id(), RowId::Committed(10));
        preferences.hide_agent_statuses = false;
        assert_eq!(transcript.display_count(&preferences), 1000);
        assert_eq!(
            transcript.display_id_at(999, &preferences),
            Some(RowId::Committed(1000))
        );
        assert_eq!(
            transcript.display_position(&RowId::Committed(1000), &preferences),
            Some(999)
        );
        assert_eq!(transcript.display_window(995, 150, &preferences).len(), 5);
    }

    fn tool_event(id: u64, agent_id: AgentId, text: &str) -> EventEnvelope {
        let mut next = event(id);
        next.agent_id = agent_id;
        next.event.kind = EventType::ToolResult(ToolResult {
            delivery: None,
            call_id: "same-call".into(),
            error: false,
            metadata: Default::default(),
            content: vec![llm_api::api::ContentBlock::Text(llm_api::api::TextBlock {
                text: text.into(),
                annotations: Vec::new(),
            })]
            .into(),
        });
        next
    }

    #[test]
    fn snapshots_bootstrap_commit_and_removal_keep_indices_without_rebuilding_history() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        transcript.apply(StreamMessage::Committed {
            event: tool_event(10, AgentId(2), "committed"),
            pending_id: None,
        });
        let committed = transcript.tool_result(AgentId(2), "same-call").unwrap();
        let previous = transcript.clone();
        transcript.apply(StreamMessage::Snapshot {
            cursor: 10,
            pending: vec![],
            queries: vec![],
        });
        assert!(Arc::ptr_eq(
            &previous.committed.entries,
            &transcript.committed.entries
        ));
        assert!(Arc::ptr_eq(
            &previous.display_index[0],
            &transcript.display_index[0]
        ));
        assert!(Arc::ptr_eq(
            &previous.tool_results,
            &transcript.tool_results
        ));

        let mut provisional = pending(1);
        provisional.kind = tool_event(11, AgentId(2), "provisional").event.kind;
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        transcript.apply(StreamMessage::Snapshot {
            cursor: 10,
            pending: vec![],
            queries: vec![],
        });
        assert_eq!(
            transcript.tool_result(AgentId(2), "same-call"),
            Some(committed.clone())
        );
        assert_eq!(
            transcript.display_count(&crate::UiPreferences::default()),
            1
        );
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        transcript.apply(StreamMessage::Committed {
            event: tool_event(11, AgentId(2), "new-commit"),
            pending_id: Some("p".into()),
        });
        assert_eq!(
            transcript
                .tool_result(AgentId(2), "same-call")
                .unwrap()
                .id(),
            RowId::Committed(11)
        );
        assert_eq!(
            transcript.display_count(&crate::UiPreferences::default()),
            2
        );

        // A stream snapshot resets retirement, allowing a fresh authoritative
        // provisional state; its removal must reveal the newest committed one.
        transcript.apply(StreamMessage::Snapshot {
            cursor: 11,
            pending: vec![provisional.clone()],
            queries: vec![],
        });
        transcript.apply(StreamMessage::Removed {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            id: "p".into(),
        });
        assert_eq!(
            transcript
                .tool_result(AgentId(2), "same-call")
                .unwrap()
                .id(),
            RowId::Committed(11)
        );
        let mut bootstrap = page(vec![]);
        bootstrap.pending = vec![provisional];
        transcript.bootstrap(bootstrap);
        assert!(
            transcript
                .tool_result(AgentId(2), "same-call")
                .unwrap()
                .is_provisional()
        );
        let mut bootstrap = page(vec![]);
        bootstrap.pending.clear();
        transcript.bootstrap(bootstrap);
        assert_eq!(
            transcript
                .tool_result(AgentId(2), "same-call")
                .unwrap()
                .id(),
            RowId::Committed(11)
        );
        assert_eq!(
            transcript.display_count(&crate::UiPreferences::default()),
            2
        );
    }

    #[test]
    fn tool_result_index_is_agent_scoped_and_preserves_newest_across_late_history() {
        let mut transcript = Transcript::new(SessionId(1), None);
        transcript.apply(StreamMessage::Committed {
            event: tool_event(10, AgentId(2), "new"),
            pending_id: None,
        });
        transcript.apply(StreamMessage::Committed {
            event: tool_event(11, AgentId(3), "other-agent"),
            pending_id: None,
        });
        let newest = transcript.tool_result(AgentId(2), "same-call").unwrap();
        transcript.merge_history(page(vec![tool_event(1, AgentId(2), "old")]));
        assert_eq!(
            transcript.tool_result(AgentId(2), "same-call"),
            Some(newest.clone())
        );
        assert_ne!(
            transcript.tool_result(AgentId(3), "same-call"),
            Some(newest.clone())
        );

        let mut provisional = pending(1);
        provisional.kind = tool_event(12, AgentId(2), "provisional").event.kind;
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        assert_eq!(
            transcript
                .tool_result(AgentId(2), "same-call")
                .unwrap()
                .id(),
            RowId::Pending("p".into())
        );
        provisional.revision = 2;
        provisional.kind = pending(2).kind;
        transcript.apply(StreamMessage::Upsert {
            pending: provisional,
        });
        assert_eq!(
            transcript.tool_result(AgentId(2), "same-call"),
            Some(newest)
        );
    }

    #[test]
    fn pending_category_changes_and_commits_keep_visible_ranks_consistent() {
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        let preferences = crate::UiPreferences::default();
        let mut provisional = pending(1);
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        assert_eq!(transcript.display_count(&preferences), 1);
        provisional.revision = 2;
        provisional.kind = EventType::StatusChange(psa_api::events::StatusChange {
            new_status: psa_api::AgentState::Idle,
            reason: None,
        });
        transcript.apply(StreamMessage::Upsert {
            pending: provisional,
        });
        assert_eq!(transcript.display_count(&preferences), 0);
        transcript.apply(StreamMessage::Committed {
            event: event(1),
            pending_id: Some("p".into()),
        });
        assert_eq!(transcript.display_count(&preferences), 1);
        assert_eq!(
            transcript.display_id_at(0, &preferences),
            Some(RowId::Committed(1))
        );
    }

    #[test]
    fn human_message_index_tracks_pending_revisions_commits_and_late_history() {
        let human = |text: &str| EventType::Message(Message::text(MessageType::Human, text));
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        let mut committed = event(5);
        committed.event.kind = human("second");
        transcript.apply(StreamMessage::Committed {
            event: committed,
            pending_id: None,
        });
        transcript.apply(StreamMessage::Committed {
            event: event(6),
            pending_id: None,
        });
        let mut provisional = pending(1);
        provisional.kind = human("third");
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        assert_eq!(
            transcript.human_message_ids(),
            [RowId::Committed(5), RowId::Pending("p".into())]
        );

        // An unchanged role keeps the shared index; a changed role leaves it.
        let snapshot = transcript.clone();
        provisional.revision = 2;
        provisional.kind = human("third, revised");
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        assert!(Arc::ptr_eq(
            &snapshot.human_messages,
            &transcript.human_messages
        ));
        provisional.revision = 3;
        provisional.kind = pending(3).kind;
        transcript.apply(StreamMessage::Upsert {
            pending: provisional.clone(),
        });
        assert_eq!(transcript.human_message_ids(), [RowId::Committed(5)]);

        provisional.revision = 4;
        provisional.kind = human("third");
        transcript.apply(StreamMessage::Upsert {
            pending: provisional,
        });
        let mut committed = event(7);
        committed.event.kind = human("third");
        transcript.apply(StreamMessage::Committed {
            event: committed,
            pending_id: Some("p".into()),
        });
        let mut older = event(1);
        older.event.kind = human("first");
        transcript.merge_history(page(vec![older]));
        // The history page's own provisional row is not part of a late merge.
        assert_eq!(
            transcript.human_message_ids(),
            [
                RowId::Committed(1),
                RowId::Committed(5),
                RowId::Committed(7)
            ]
        );
        let first = transcript.row_payload(&RowId::Committed(1)).unwrap();
        assert!(
            matches!(first.kind(), EventType::Message(message) if message.message_type == MessageType::Human)
        );
        assert!(
            transcript
                .row_payload(&RowId::Pending("p".into()))
                .is_none()
        );
    }

    fn inference_end(id: u64, outcome: llm_api::api::Outcome) -> EventEnvelope {
        let mut next = event(id);
        next.event.kind = EventType::InferenceResultEnd(psa_api::events::InferenceResultEnd {
            id,
            outcome,
            usage: Default::default(),
            request_time: std::time::Duration::ZERO,
            response_id: None,
            provider_metadata: Default::default(),
        });
        next
    }

    #[test]
    fn turns_count_from_the_preceding_user_message_and_dividers_follow_the_preference() {
        use llm_api::api::{Outcome, StopReason};
        let completed = || Outcome::Completed {
            reason: StopReason::ToolCalls,
        };
        let human = |id| {
            let mut next = event(id);
            next.event.kind = EventType::Message(Message::text(MessageType::Human, "question"));
            next
        };
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        for next in [
            inference_start(11),
            paired_end(11, 12, completed()),
            inference_start(13),
            paired_end(13, 14, Outcome::Cancelled),
            human(15),
            inference_start(16),
            paired_end(16, 17, completed()),
        ] {
            transcript.apply(StreamMessage::Committed {
                event: next,
                pending_id: None,
            });
        }
        let turn = |transcript: &Transcript, id| {
            let turn = transcript.turn_number(&RowId::Committed(id))?;
            Some((turn.number, turn.exact))
        };
        assert_eq!(turn(&transcript, 11), Some((1, true)));
        assert_eq!(turn(&transcript, 13), Some((2, true)));
        assert_eq!(turn(&transcript, 12), None);
        assert_eq!(turn(&transcript, 15), None);
        assert_eq!(turn(&transcript, 16), Some((1, true)));

        // Unloaded history may hold earlier turns, until a user message bounds them.
        transcript.has_older = true;
        assert_eq!(turn(&transcript, 13), Some((2, false)));
        assert_eq!(turn(&transcript, 16), Some((1, true)));
        transcript.merge_history(page(vec![inference_start(9)]));
        assert_eq!(turn(&transcript, 13), Some((3, false)));
        transcript.merge_history(page(vec![human(8)]));
        assert_eq!(turn(&transcript, 13), Some((3, true)));

        // A turn's divider is its start; only an unsuccessful end is a row.
        let mut preferences = crate::UiPreferences::default();
        assert_eq!(
            preferences.turn_dividers,
            crate::TurnDividerDisplay::Labeled
        );
        assert_eq!(transcript.display_count(&preferences), 7);
        preferences.turn_dividers = crate::TurnDividerDisplay::Line;
        assert_eq!(transcript.display_count(&preferences), 7);
        preferences.turn_dividers = crate::TurnDividerDisplay::Hidden;
        assert_eq!(transcript.display_count(&preferences), 3);
        assert_eq!(
            transcript.display_id_at(1, &preferences),
            Some(RowId::Committed(14))
        );

        // A start finds its own end, once the inference has ended.
        let end = |id| transcript.inference_end(&RowId::Committed(id));
        assert_eq!(end(13).map(|end| end.id()), Some(RowId::Committed(14)));
        assert!(end(9).is_none());
        assert!(end(12).is_none());
    }

    fn inference_start(id: u64) -> EventEnvelope {
        let mut next = event(id);
        next.event.kind = EventType::InferenceResultStart(psa_api::events::InferenceResultStart {
            id,
            model_config: llm_api::providers::ModelConfig {
                model: llm_api::providers::Model::new(
                    llm_api::providers::ProviderType::Openrouter,
                    "test",
                )
                .unwrap(),
                effort: llm_api::providers::Effort::High,
            },
            request_config: Default::default(),
        });
        next
    }

    /// An inference end paired with the start of the same inference.
    fn paired_end(start: u64, id: u64, outcome: llm_api::api::Outcome) -> EventEnvelope {
        let mut next = inference_end(id, outcome);
        if let EventType::InferenceResultEnd(end) = &mut next.event.kind {
            end.id = start;
        }
        next
    }

    #[test]
    fn rows_resolve_to_the_first_context_they_were_sent_in() {
        use llm_api::api::Outcome;
        let mut transcript = Transcript::new(SessionId(1), Some(AgentId(2)));
        let mut reset = event(17);
        reset.event.kind = EventType::NewContext;
        for next in [
            event(1),
            inference_start(2),
            event(3),
            paired_end(2, 4, Outcome::Cancelled),
            event(5),
            inference_start(6),
            event(7),
            paired_end(6, 8, Outcome::Cancelled),
            event(9),
            inference_start(10),
            paired_end(10, 11, Outcome::Cancelled),
            event(12),
            reset,
            inference_start(18),
            event(19),
            paired_end(18, 20, Outcome::Cancelled),
            event(21),
        ] {
            transcript.apply(StreamMessage::Committed {
                event: next,
                pending_id: None,
            });
        }
        let turn = |id| transcript.context_turn(&RowId::Committed(id));
        // Input is first sent to the inference that starts after it.
        assert_eq!(turn(1), Some(4));
        assert_eq!(turn(5), Some(8));
        // A turn's output is first sent to the next turn.
        assert_eq!(turn(3), Some(8));
        assert_eq!(turn(7), Some(11));
        // An inference's start and end stand for its own turn.
        assert_eq!(turn(2), Some(4));
        assert_eq!(turn(4), Some(4));
        assert_eq!(turn(18), Some(20));
        assert_eq!(turn(8), Some(8));
        // A new context opened before the row was ever sent.
        assert_eq!(turn(12), None);
        // Nothing has been sent to the model after the last inference started.
        assert_eq!(turn(19), None);
        assert_eq!(turn(21), None);
        assert_eq!(turn(99), None);
        assert_eq!(transcript.latest_turn(), Some(RowId::Committed(20)));
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
