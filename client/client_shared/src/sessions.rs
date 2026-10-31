use psa_api::{GlobalStreamMessage, ProbeId, Session, SessionId, SessionsPage};
use std::{collections::HashMap, path::PathBuf};
#[derive(Debug, Clone)]
pub struct SessionGroup {
    pub probe_id: ProbeId,
    pub working_directory: PathBuf,
    pub sessions: Vec<Session>,
    pub busy: bool,
    /// Groups contain loaded summaries only, not necessarily every session in this folder.
    pub complete: bool,
}
#[derive(Debug, Clone, Default)]
pub struct SessionIndex {
    pub sessions: HashMap<SessionId, Session>,
    pub next_before: Option<String>,
    pub needs_resync: bool,
    pub ready: bool,
    pub fully_loaded: bool,
    generation: u64,
    revisions: HashMap<SessionId, u64>,
}
impl SessionIndex {
    /// Cancel old requests/subscriptions first; retain offline cache until replacement succeeds.
    pub fn begin_bootstrap(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.revisions.clear();
        self.ready = false;
        self.needs_resync = false;
        self.fully_loaded = false;
        self.next_before = None;
        self.generation
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn upsert(&mut self, session: Session) {
        self.sessions.insert(session.id, session);
    }
    fn apply_revision(&mut self, session: Session, revision: u64) {
        if self
            .revisions
            .get(&session.id)
            .is_none_or(|r| *r <= revision)
        {
            self.revisions.insert(session.id, revision);
            self.upsert(session);
        }
    }
    /// Associate every HTTP request with its bootstrap generation. A late response is ignored.
    pub fn merge_page(&mut self, generation: u64, page: SessionsPage) -> bool {
        if generation != self.generation || !self.ready || self.needs_resync {
            return false;
        }
        for session in page.sessions {
            self.apply_revision(session, page.revision);
        }
        self.next_before = page.next_before;
        self.fully_loaded = self.next_before.is_none();
        true
    }
    pub fn apply_global(&mut self, generation: u64, message: GlobalStreamMessage) {
        if generation != self.generation {
            return;
        }
        match message {
            GlobalStreamMessage::Ready { .. } => self.ready = true,
            GlobalStreamMessage::SessionUpdated { revision, session } => {
                if self.ready && !self.needs_resync {
                    self.apply_revision(session, revision);
                }
            }
            GlobalStreamMessage::ResyncRequired => {
                self.needs_resync = true;
                self.ready = false;
            }
        }
    }
    pub fn groups(&self, probe: Option<&ProbeId>) -> Vec<SessionGroup> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|s| probe.is_none_or(|p| &s.probe_id == p))
            .collect();
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.0.cmp(&a.id.0))
        });
        let mut groups: Vec<SessionGroup> = Vec::new();
        let mut group_indices = HashMap::new();
        for session in sessions {
            // Hash the original wire spelling, not Path's component-normalizing equality.
            let key = (session.probe_id, session.working_directory.as_os_str());
            let index = *group_indices.entry(key).or_insert_with(|| {
                let index = groups.len();
                groups.push(SessionGroup {
                    probe_id: session.probe_id,
                    working_directory: session.working_directory.clone(),
                    busy: false,
                    sessions: Vec::new(),
                    complete: self.fully_loaded,
                });
                index
            });
            let group = &mut groups[index];
            group.busy |= session.busy;
            group.sessions.push(session.clone());
        }
        // First appearance is newest; deterministic session-ID order also breaks group ties.
        groups
    }
}
