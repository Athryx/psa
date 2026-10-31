//! Canonical Rust snapshots plus explicit browser mutations; never edit echoes.
use super::{InputMutation, InputSnapshot, next_mutation_id};
use client_shared::TranscriptTarget;
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DRAFT: AtomicU64 = AtomicU64::new(1);
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct DraftMetadata {
    browser_key: String,
    revision: u64,
    snapshot: Option<InputSnapshot>,
    mounts: HashMap<u64, (u64, bool)>,
    mutation: Option<InputMutation>,
    pending_initial: Option<String>,
}

impl Default for DraftMetadata {
    fn default() -> Self {
        Self {
            browser_key: format!("draft:{}", NEXT_DRAFT.fetch_add(1, Ordering::Relaxed)),
            revision: 0,
            snapshot: None,
            mounts: HashMap::new(),
            mutation: None,
            pending_initial: None,
        }
    }
}

/// The map-like methods are intentional mutations. Browser input MUST use
/// `apply_snapshot` instead of `insert`, otherwise an edit becomes an echo.
#[derive(Clone, Debug)]
pub(crate) struct DraftStore<K = TranscriptTarget> {
    values: HashMap<K, String>,
    metadata: HashMap<K, DraftMetadata>,
    retired_mounts: HashSet<u64>,
}

impl<K> Default for DraftStore<K> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            metadata: HashMap::new(),
            retired_mounts: HashSet::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> DraftStore<K> {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, key: &K) -> Option<&String> {
        self.values.get(key)
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.values.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) fn revision(&self, key: &K) -> u64 {
        self.metadata.get(key).map_or(0, |meta| meta.revision)
    }

    pub(crate) fn browser_key(&self, key: &K) -> Option<&str> {
        self.metadata.get(key).map(|meta| meta.browser_key.as_str())
    }

    /// Establish once per target. Promotion preserves this identity so browser
    /// edits not yet received by Rust survive the old component unmounting.
    pub(crate) fn ensure_browser_key(&mut self, key: K) -> String {
        self.metadata.entry(key).or_default().browser_key.clone()
    }

    pub(crate) fn mutation(&self, key: &K) -> Option<InputMutation> {
        self.metadata.get(key)?.mutation.clone()
    }

    pub(crate) fn snapshot(&self, key: &K) -> Option<&InputSnapshot> {
        self.metadata.get(key)?.snapshot.as_ref()
    }

    pub(crate) fn insert(&mut self, key: K, text: String) -> Option<String> {
        let previous = self.values.insert(key.clone(), text.clone());
        self.replace(key, text);
        previous
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        let previous = self.values.remove(key);
        self.replace(key.clone(), String::new());
        previous
    }

    fn replace(&mut self, key: K, text: String) {
        let meta = self.metadata.entry(key).or_default();
        meta.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
        meta.pending_initial = meta.snapshot.is_none().then(|| text.clone());
        meta.mutation = meta.snapshot.as_ref().map(|snapshot| InputMutation {
            id: next_mutation_id(),
            expected: snapshot.revision,
            text,
            selection: None,
            focus: false,
        });
    }

    /// A send result may clear only its original revision, not merely equal text.
    /// The browser independently rechecks its revision for edits still in flight.
    pub(crate) fn clear_if_revision(&mut self, key: &K, revision: u64) -> bool {
        if self.revision(key) != revision {
            return false;
        }
        self.remove(key);
        true
    }

    /// Return false for a stale packet. Callers must not run a stale key command.
    /// Selection-only snapshots are accepted without advancing the draft revision.
    pub(crate) fn apply_snapshot(&mut self, key: K, snapshot: InputSnapshot) -> bool {
        if self.retired_mounts.contains(&snapshot.revision.mount) {
            return false;
        }
        let meta = self.metadata.entry(key.clone()).or_default();
        let revision = snapshot.revision;
        if let Some((sequence, native)) = meta.mounts.get(&revision.mount) {
            // The asynchronous installer can start after additional fallback
            // events beyond its captured seed. Allow exactly one clock-zero to
            // native handoff; the native DOM snapshot is authoritative then.
            let handoff = revision.clock > 0 && !native;
            if revision.sequence < *sequence && !handoff {
                return false;
            }
        }
        if let Some(previous) = &meta.snapshot {
            if (revision.clock, snapshot.applied) < (previous.revision.clock, previous.applied) {
                return false;
            }
        }
        meta.mounts
            .insert(revision.mount, (revision.sequence, revision.clock > 0));

        let previous_revision = meta.snapshot.as_ref().map(|s| s.revision);
        if let Some(text) = meta.pending_initial.take() {
            // A click can intentionally replace a field before its mount packet
            // crosses IPC. Bind that intent only if no user edit has occurred.
            if text != snapshot.text
                && revision.sequence == 0
                && !snapshot.is_edit()
                && !snapshot.composing
            {
                meta.mutation = Some(snapshot.replacement(text));
            }
        }
        let pending = meta.mutation.as_ref();
        let acknowledged = pending.is_some_and(|mutation| {
            snapshot.response == Some(mutation.id) || snapshot.applied >= mutation.id
        });
        let superseded = pending.is_some_and(|mutation| revision.clock > mutation.expected.clock);
        let pending_not_applied = pending.is_some() && !acknowledged && !superseded;
        meta.snapshot = Some(snapshot.clone());

        // An old selection/key packet must not roll back a pending intentional
        // replacement. Its eventual ACK (success OR rejection) resolves it.
        if pending_not_applied {
            return true;
        }
        if acknowledged || superseded {
            meta.mutation = None;
        }
        let previous_text = self
            .values
            .get(&key)
            .map(String::as_str)
            .unwrap_or_default();
        let edited = previous_revision.is_some_and(|previous| {
            previous.clock != revision.clock
                || (previous.mount == revision.mount && previous.sequence != revision.sequence)
        });
        if edited || previous_text != snapshot.text {
            meta.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
        }
        if snapshot.text.is_empty() {
            self.values.remove(&key);
        } else {
            self.values.insert(key, snapshot.text);
        }
        true
    }

    /// An initialized editor may already contain a browser edit Rust has not
    /// received, even if its last Rust snapshot is empty or it has just closed.
    pub(crate) fn has_editor(&self, key: &K) -> bool {
        self.metadata.contains_key(key)
    }

    pub(crate) fn can_transfer(&self, from: &K, to: &K) -> bool {
        from == to || (!self.has_editor(to) && !self.values.contains_key(to))
    }

    /// Promotion preserves the shared browser draft identity and captured send
    /// revision, while retiring old editor callbacks and outstanding commands.
    /// A conflict leaves BOTH drafts untouched; callers must keep the draft view
    /// open so the user can explicitly copy/merge rather than lose unseen edits.
    pub(crate) fn transfer(&mut self, from: &K, to: K) -> bool {
        if !self.can_transfer(from, &to) {
            return false;
        }
        if *from == to {
            return true;
        }
        let text = self.values.remove(from);
        let metadata = self.metadata.remove(from);
        if let Some(meta) = &metadata {
            self.retired_mounts.extend(meta.mounts.keys().copied());
        }
        if let Some(mut meta) = metadata {
            // Keep the captured submission revision AND its original browser
            // guard. Dropping the guard would let a later clear bind to unseen
            // browser edits observed only after the promotion.
            meta.mounts.clear();
            meta.mutation = None;
            meta.pending_initial = None;
            self.metadata.insert(to.clone(), meta);
        }
        if let Some(text) = text {
            self.values.insert(to, text);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputRevision, InputSelection, SnapshotCause};

    fn snapshot(mount: u64, sequence: u64, clock: u64, text: &str) -> InputSnapshot {
        InputSnapshot {
            revision: InputRevision {
                mount,
                sequence,
                clock,
            },
            text: text.into(),
            selection: InputSelection::default(),
            composing: false,
            applied: 0,
            response: None,
            cause: SnapshotCause::Edit,
        }
    }

    #[test]
    fn edit_snapshots_never_become_mutations_and_old_frames_are_rejected() {
        let mut drafts = DraftStore::new();
        assert!(drafts.apply_snapshot(1, snapshot(10, 1, 1, "abc")));
        assert!(drafts.apply_snapshot(1, snapshot(10, 2, 2, "ac")));
        assert!(!drafts.apply_snapshot(1, snapshot(10, 1, 1, "abc")));
        assert_eq!(drafts.get(&1).unwrap(), "ac");
        assert!(drafts.mutation(&1).is_none());
    }

    #[test]
    fn same_text_after_edit_is_not_the_submitted_revision() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 1, 1, "abc"));
        let sent = drafts.revision(&1);
        drafts.apply_snapshot(1, snapshot(10, 2, 2, "ab"));
        drafts.apply_snapshot(1, snapshot(10, 3, 3, "abc"));
        assert!(!drafts.clear_if_revision(&1, sent));
        assert_eq!(drafts.get(&1).unwrap(), "abc");
    }

    #[test]
    fn browser_rejection_restores_an_optimistic_clear() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 1, 1, "sent"));
        assert!(drafts.clear_if_revision(&1, drafts.revision(&1)));
        let mutation = drafts.mutation(&1).unwrap();
        assert!(drafts.get(&1).is_none());
        let mut rejection = snapshot(10, 2, 2, "sent and newer");
        rejection.response = Some(mutation.id);
        rejection.cause = SnapshotCause::Mutation;
        drafts.apply_snapshot(1, rejection);
        assert_eq!(drafts.get(&1).unwrap(), "sent and newer");
        assert!(drafts.mutation(&1).is_none());
    }

    #[test]
    fn selection_before_mutation_ack_does_not_restore_old_text() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 1, 1, "/mo"));
        drafts.insert(1, "/model ".into());
        let mutation = drafts.mutation(&1).unwrap();
        let mut selection = snapshot(10, 1, 1, "/mo");
        selection.cause = SnapshotCause::Selection;
        drafts.apply_snapshot(1, selection);
        assert_eq!(drafts.get(&1).unwrap(), "/model ");
        let mut ack = snapshot(10, 1, 1, "/model ");
        ack.applied = mutation.id;
        ack.response = Some(mutation.id);
        ack.cause = SnapshotCause::Mutation;
        drafts.apply_snapshot(1, ack);
        assert!(drafts.mutation(&1).is_none());
    }

    #[test]
    fn an_intent_before_mount_is_bound_only_if_the_browser_has_not_edited() {
        let mut drafts = DraftStore::new();
        drafts.insert(1, "chosen answer".into());
        let mut mounted = snapshot(10, 0, 1, "");
        mounted.cause = SnapshotCause::Mount;
        drafts.apply_snapshot(1, mounted);
        assert_eq!(drafts.get(&1).unwrap(), "chosen answer");
        assert_eq!(drafts.mutation(&1).unwrap().expected.clock, 1);

        let mut edited = DraftStore::new();
        edited.insert(1, "chosen answer".into());
        edited.apply_snapshot(1, snapshot(10, 1, 1, "new user answer"));
        assert_eq!(edited.get(&1).unwrap(), "new user answer");
        assert!(edited.mutation(&1).is_none());
    }

    #[test]
    fn fallback_edits_advance_revisions_and_handoff_once_to_native_sequence() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 3, 0, "same text"));
        let sent = drafts.revision(&1);
        drafts.apply_snapshot(1, snapshot(10, 4, 0, "same text"));
        assert_ne!(drafts.revision(&1), sent);
        // Installer captured sequence zero before four fallback edits arrived.
        assert!(drafts.apply_snapshot(1, snapshot(10, 1, 1, "latest browser text")));
        assert!(drafts.apply_snapshot(1, snapshot(10, 2, 2, "next browser text")));
        assert!(!drafts.apply_snapshot(1, snapshot(10, 1, 1, "stale native text")));
        assert!(!drafts.apply_snapshot(1, snapshot(10, 5, 0, "late fallback text")));
        assert_eq!(drafts.get(&1).unwrap(), "next browser text");
    }

    #[test]
    fn promotion_preserves_identity_and_send_revision_but_never_rebinds_a_stale_clear() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 1, 1, "first message"));
        let identity = drafts.browser_key(&1).unwrap().to_owned();
        let submitted = drafts.revision(&1);
        drafts.transfer(&1, 2);
        assert_eq!(drafts.browser_key(&2), Some(identity.as_str()));
        assert_eq!(drafts.revision(&2), submitted);
        assert!(!drafts.contains_key(&1));
        assert!(drafts.clear_if_revision(&2, submitted));
        let clear = drafts.mutation(&2).unwrap();
        assert_eq!(
            clear.expected,
            InputRevision {
                mount: 10,
                sequence: 1,
                clock: 1
            }
        );
        assert!(!drafts.apply_snapshot(2, snapshot(10, 2, 2, "late old callback")));
        let mut remounted = snapshot(20, 0, 2, "unseen browser edit survives promotion");
        remounted.cause = SnapshotCause::Mount;
        remounted.response = Some(clear.id);
        drafts.apply_snapshot(2, remounted);
        assert_eq!(
            drafts.get(&2).unwrap(),
            "unseen browser edit survives promotion"
        );
        assert!(drafts.mutation(&2).is_none());
    }

    #[test]
    fn promotion_conflict_leaves_both_nonempty_drafts_untouched() {
        let mut drafts = DraftStore::new();
        drafts.insert(1, "source".into());
        drafts.insert(2, "destination".into());
        let source_identity = drafts.browser_key(&1).unwrap().to_owned();
        let identity = drafts.browser_key(&2).unwrap().to_owned();
        let source_revision = drafts.revision(&1);
        let revision = drafts.revision(&2);
        assert!(!drafts.can_transfer(&1, &2));
        assert!(!drafts.transfer(&1, 2));
        assert_eq!(drafts.get(&1).unwrap(), "source");
        assert_eq!(drafts.browser_key(&1), Some(source_identity.as_str()));
        assert_eq!(drafts.revision(&1), source_revision);
        assert_eq!(drafts.get(&2).unwrap(), "destination");
        assert_eq!(drafts.browser_key(&2), Some(identity.as_str()));
        assert_eq!(drafts.revision(&2), revision);
    }

    #[test]
    fn an_empty_initialized_destination_cannot_lose_an_unseen_browser_edit() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 1, 1, "source draft"));
        let mut empty_destination = snapshot(20, 0, 2, "");
        empty_destination.cause = SnapshotCause::Mount;
        drafts.apply_snapshot(2, empty_destination);
        let source_identity = drafts.browser_key(&1).unwrap().to_owned();
        let destination_identity = drafts.browser_key(&2).unwrap().to_owned();
        assert!(drafts.get(&2).is_none());
        assert!(drafts.has_editor(&2));
        assert!(!drafts.transfer(&1, 2));
        assert_eq!(drafts.get(&1).unwrap(), "source draft");
        assert_eq!(drafts.browser_key(&1), Some(source_identity.as_str()));
        assert_eq!(drafts.browser_key(&2), Some(destination_identity.as_str()));
        // This packet was still queued when promotion was attempted. The editor
        // identity survives and its user edit is accepted rather than displaced.
        assert!(drafts.apply_snapshot(2, snapshot(20, 1, 3, "unseen destination edit")));
        assert_eq!(drafts.get(&2).unwrap(), "unseen destination edit");
        assert_eq!(drafts.get(&1).unwrap(), "source draft");
    }

    #[test]
    fn duplicate_editors_are_ordered_by_global_clock_not_mount_order() {
        let mut drafts = DraftStore::new();
        drafts.apply_snapshot(1, snapshot(10, 3, 10, "left"));
        drafts.apply_snapshot(1, snapshot(20, 1, 11, "right"));
        let sent = drafts.revision(&1);
        assert!(drafts.apply_snapshot(1, snapshot(10, 4, 12, "left again")));
        assert!(!drafts.apply_snapshot(1, snapshot(20, 1, 11, "right")));
        assert!(!drafts.clear_if_revision(&1, sent));
        assert_eq!(drafts.get(&1).unwrap(), "left again");
    }
}
