use psa::api::{AgentId, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TranscriptTarget {
    pub session_id: SessionId,
    pub agent_id: AgentId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ViewId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy)]
struct GroupRect {
    id: GroupId,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}
/// What a tab shows of its target agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TabKind {
    #[default]
    Transcript,
    /// The makeup of the context sent for one model turn, identified by the
    /// agent event ID of its inference end.
    Context { turn: u64 },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: ViewId,
    pub target: TranscriptTarget,
    #[serde(default)]
    pub kind: TabKind,
    pub preview: bool,
    pub last_focused: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: GroupId,
    pub tabs: Vec<Tab>,
    pub active: Option<ViewId>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SplitNode {
    Group(TabGroup),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub root: SplitNode,
    pub focused_group: GroupId,
    next_identity: u64,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            root: SplitNode::Group(TabGroup {
                id: GroupId(1),
                tabs: Vec::new(),
                active: None,
            }),
            focused_group: GroupId(1),
            next_identity: 2,
        }
    }
}
impl SplitNode {
    fn collect_rects(&self, rect: GroupRect, out: &mut Vec<GroupRect>) {
        match self {
            Self::Group(group) => out.push(GroupRect {
                id: group.id,
                ..rect
            }),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let ratio = if ratio.is_finite() {
                    ratio.clamp(0.1, 0.9)
                } else {
                    0.5
                };
                let (mut first_rect, mut second_rect) = (rect, rect);
                match axis {
                    SplitAxis::Horizontal => {
                        let x = rect.left + (rect.right - rect.left) * ratio;
                        first_rect.right = x;
                        second_rect.left = x;
                    }
                    SplitAxis::Vertical => {
                        let y = rect.top + (rect.bottom - rect.top) * ratio;
                        first_rect.bottom = y;
                        second_rect.top = y;
                    }
                }
                first.collect_rects(first_rect, out);
                second.collect_rects(second_rect, out);
            }
        }
    }
    fn collect<'a>(&'a self, out: &mut Vec<&'a TabGroup>) {
        match self {
            Self::Group(group) => out.push(group),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }
    fn replace_target(&mut self, from: TranscriptTarget, to: TranscriptTarget) {
        match self {
            Self::Group(group) => {
                for tab in &mut group.tabs {
                    if tab.target == from {
                        tab.target = to;
                    }
                }
            }
            Self::Split { first, second, .. } => {
                first.replace_target(from, to);
                second.replace_target(from, to);
            }
        }
    }
    fn group_mut(&mut self, id: GroupId) -> Option<&mut TabGroup> {
        match self {
            Self::Group(group) => (group.id == id).then_some(group),
            Self::Split { first, second, .. } => {
                first.group_mut(id).or_else(|| second.group_mut(id))
            }
        }
    }
    fn split(&mut self, id: GroupId, axis: SplitAxis, new: GroupId) -> bool {
        match self {
            Self::Group(group) if group.id == id => {
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Group(group.clone())),
                    second: Box::new(Self::Group(TabGroup {
                        id: new,
                        tabs: Vec::new(),
                        active: None,
                    })),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(id, axis, new) || second.split(id, axis, new)
            }
            _ => false,
        }
    }
    // Remove only the requested leaf, returning a group in its sibling subtree.
    fn remove_group(&mut self, id: GroupId) -> Option<GroupId> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        let sibling = if matches!(&**first, Self::Group(g) if g.id == id) {
            Some((**second).clone())
        } else if matches!(&**second, Self::Group(g) if g.id == id) {
            Some((**first).clone())
        } else {
            return first.remove_group(id).or_else(|| second.remove_group(id));
        };
        let sibling = sibling.unwrap();
        let mut groups = Vec::new();
        sibling.collect(&mut groups);
        let focus = groups[0].id;
        *self = sibling;
        Some(focus)
    }
}
impl Workspace {
    pub fn groups(&self) -> Vec<&TabGroup> {
        let mut out = Vec::new();
        self.root.collect(&mut out);
        out
    }
    /// Retarget every view while preserving its identity, focus, and preview state.
    pub fn replace_target(&mut self, from: TranscriptTarget, to: TranscriptTarget) {
        self.root.replace_target(from, to);
    }
    fn next_id(&mut self) -> u64 {
        let observed = self
            .groups()
            .iter()
            .flat_map(|g| std::iter::once(g.id.0).chain(g.tabs.iter().map(|t| t.id.0)))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let id = observed.max(self.next_identity);
        self.next_identity = id.saturating_add(1);
        id
    }
    fn next_focus(&self) -> u64 {
        self.groups()
            .iter()
            .flat_map(|g| &g.tabs)
            .map(|t| t.last_focused)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }
    pub fn active_tab(&self) -> Option<&Tab> {
        let group = self
            .groups()
            .into_iter()
            .find(|g| g.id == self.focused_group)?;
        group.tabs.iter().find(|t| Some(t.id) == group.active)
    }
    /// Focus a zero-based tab index in the focused group.
    pub fn focus_tab_at(&mut self, index: usize) -> bool {
        let view = self
            .groups()
            .into_iter()
            .find(|group| group.id == self.focused_group)
            .and_then(|group| group.tabs.get(index))
            .map(|tab| tab.id);
        view.is_some_and(|view| self.focus(view))
    }

    /// Focus the closest pane in a direction, preferring panes that overlap on
    /// the perpendicular axis. Empty panes are navigable too.
    pub fn focus_direction(&mut self, direction: FocusDirection) -> bool {
        let mut rects = Vec::new();
        self.root.collect_rects(
            GroupRect {
                id: self.focused_group,
                left: 0.0,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            },
            &mut rects,
        );
        let Some(source) = rects
            .iter()
            .find(|rect| rect.id == self.focused_group)
            .copied()
        else {
            return false;
        };
        let mut candidates = rects
            .into_iter()
            .filter_map(|rect| {
                if rect.id == source.id {
                    return None;
                }
                let (primary, perpendicular) = match direction {
                    FocusDirection::Right if rect.left >= source.right - 1e-6 => (
                        rect.left - source.right,
                        (source.top, source.bottom, rect.top, rect.bottom),
                    ),
                    FocusDirection::Left if rect.right <= source.left + 1e-6 => (
                        source.left - rect.right,
                        (source.top, source.bottom, rect.top, rect.bottom),
                    ),
                    FocusDirection::Down if rect.top >= source.bottom - 1e-6 => (
                        rect.top - source.bottom,
                        (source.left, source.right, rect.left, rect.right),
                    ),
                    FocusDirection::Up if rect.bottom <= source.top + 1e-6 => (
                        source.top - rect.bottom,
                        (source.left, source.right, rect.left, rect.right),
                    ),
                    _ => return None,
                };
                let (a, b, c, d) = perpendicular;
                let gap = (a - d).max(c - b).max(0.0);
                let center_distance = ((a + b) - (c + d)).abs();
                Some((rect.id, gap, primary.max(0.0), center_distance))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then(a.2.total_cmp(&b.2))
                .then(a.3.total_cmp(&b.3))
        });
        let Some((id, ..)) = candidates.first().copied() else {
            return false;
        };
        let active = self
            .groups()
            .into_iter()
            .find(|group| group.id == id)
            .and_then(|group| group.active);
        if let Some(view) = active {
            self.focus(view)
        } else {
            self.focused_group = id;
            true
        }
    }

    pub fn focus(&mut self, view: ViewId) -> bool {
        let Some(group) = self
            .groups()
            .into_iter()
            .find(|g| g.tabs.iter().any(|t| t.id == view))
            .map(|g| g.id)
        else {
            return false;
        };
        let clock = self.next_focus();
        let g = self.root.group_mut(group).unwrap();
        g.active = Some(view);
        g.tabs
            .iter_mut()
            .find(|t| t.id == view)
            .unwrap()
            .last_focused = clock;
        self.focused_group = group;
        true
    }
    pub fn open(&mut self, target: TranscriptTarget, permanent: bool) -> ViewId {
        let existing = self
            .groups()
            .iter()
            .flat_map(|g| &g.tabs)
            .filter(|t| t.target == target && t.kind == TabKind::Transcript)
            .max_by_key(|t| (t.last_focused, t.id.0))
            .map(|t| t.id);
        if let Some(id) = existing {
            self.focus(id);
            if permanent {
                self.promote(id);
            }
            return id;
        }
        self.open_duplicate(target, permanent)
    }
    /// Show one turn's context for an agent, reusing the agent's context tab.
    pub fn open_context(&mut self, target: TranscriptTarget, turn: u64) -> ViewId {
        let kind = TabKind::Context { turn };
        let existing = self
            .groups()
            .iter()
            .flat_map(|g| &g.tabs)
            .filter(|t| t.target == target && matches!(t.kind, TabKind::Context { .. }))
            .max_by_key(|t| (t.last_focused, t.id.0))
            .map(|t| t.id);
        let Some(id) = existing else {
            return self.open_tab(target, kind, true);
        };

        for group in self.groups().iter().map(|g| g.id).collect::<Vec<_>>() {
            let tabs = &mut self.root.group_mut(group).unwrap().tabs;
            if let Some(tab) = tabs.iter_mut().find(|t| t.id == id) {
                tab.kind = kind;
            }
        }
        self.focus(id);
        id
    }
    /// Internal/future duplicate support; ordinary navigation must use `open`.
    pub fn open_duplicate(&mut self, target: TranscriptTarget, permanent: bool) -> ViewId {
        self.open_tab(target, TabKind::Transcript, permanent)
    }
    fn open_tab(&mut self, target: TranscriptTarget, kind: TabKind, permanent: bool) -> ViewId {
        let id = ViewId(self.next_id());
        let clock = self.next_focus();
        if self.root.group_mut(self.focused_group).is_none() {
            self.focused_group = self.groups()[0].id;
        }
        let group = self.root.group_mut(self.focused_group).unwrap();
        let tab = Tab {
            id,
            target,
            kind,
            preview: !permanent,
            last_focused: clock,
        };
        if !permanent && let Some(index) = group.tabs.iter().position(|t| t.preview) {
            group.tabs[index] = tab;
        } else {
            group.tabs.push(tab);
        }
        group.active = Some(id);
        id
    }
    /// Call before editing a draft or submitting a request.
    pub fn promote(&mut self, view: ViewId) {
        for id in self.groups().iter().map(|g| g.id).collect::<Vec<_>>() {
            if let Some(t) = self
                .root
                .group_mut(id)
                .unwrap()
                .tabs
                .iter_mut()
                .find(|t| t.id == view)
            {
                t.preview = false;
                return;
            }
        }
    }
    pub fn split(&mut self, group: GroupId, axis: SplitAxis) -> Option<GroupId> {
        let new = GroupId(self.next_id());
        self.root.split(group, axis, new).then_some(new)
    }
    pub fn close(&mut self, view: ViewId) -> bool {
        let Some(id) = self
            .groups()
            .into_iter()
            .find(|g| g.tabs.iter().any(|t| t.id == view))
            .map(|g| g.id)
        else {
            return false;
        };
        let g = self.root.group_mut(id).unwrap();
        let index = g.tabs.iter().position(|t| t.id == view).unwrap();
        g.tabs.remove(index);
        if g.active == Some(view) {
            g.active = g
                .tabs
                .get(index.min(g.tabs.len().saturating_sub(1)))
                .map(|t| t.id);
        }
        if g.tabs.is_empty() {
            self.close_empty_group(id);
        }
        true
    }
    /// Close an empty pane, retaining the final pane and all unrelated panes.
    pub fn close_empty_group(&mut self, id: GroupId) -> bool {
        if !self
            .groups()
            .iter()
            .any(|g| g.id == id && g.tabs.is_empty())
        {
            return false;
        }
        let Some(fallback) = self.root.remove_group(id) else {
            return false;
        };
        if self.focused_group == id {
            self.focused_group = fallback;
        }
        true
    }
    /// Close the active tab, or the focused pane if it is empty.
    pub fn close_active(&mut self) -> bool {
        if let Some(view) = self.active_tab().map(|tab| tab.id) {
            self.close(view)
        } else {
            self.close_empty_group(self.focused_group)
        }
    }
    pub fn move_tab(&mut self, view: ViewId, destination: GroupId, index: usize) -> bool {
        if !self.groups().iter().any(|g| g.id == destination) {
            return false;
        }
        let Some(source) = self
            .groups()
            .into_iter()
            .find(|g| g.tabs.iter().any(|t| t.id == view))
            .map(|g| g.id)
        else {
            return false;
        };
        let group = self.root.group_mut(source).unwrap();
        let pos = group.tabs.iter().position(|t| t.id == view).unwrap();
        let tab = group.tabs.remove(pos);
        if group.active == Some(view) {
            group.active = group.tabs.first().map(|t| t.id);
        }
        let group = self.root.group_mut(destination).unwrap();
        if tab.preview {
            for old in &mut group.tabs {
                old.preview = false;
            }
        }
        group.tabs.insert(index.min(group.tabs.len()), tab);
        self.focus(view);
        if source != destination {
            self.close_empty_group(source);
        }
        true
    }
    /// Moves a tab to an insertion `slot` between the destination's current tabs,
    /// counting the moved tab itself when reordering within its own pane.
    pub fn move_tab_to_slot(&mut self, view: ViewId, destination: GroupId, slot: usize) -> bool {
        let current = self
            .groups()
            .into_iter()
            .find(|g| g.id == destination)
            .and_then(|g| g.tabs.iter().position(|t| t.id == view));
        let index = match current {
            Some(position) if position < slot => slot - 1,
            _ => slot,
        };
        self.move_tab(view, destination, index)
    }
    /// Repairs malformed layout references without dropping unavailable remote targets.
    pub fn restore(&mut self) {
        let mut used = std::collections::HashSet::new();
        let mut next = self.next_id();
        fn repair(node: &mut SplitNode, used: &mut std::collections::HashSet<u64>, next: &mut u64) {
            match node {
                SplitNode::Group(g) => {
                    if !used.insert(g.id.0) {
                        g.id = GroupId(*next);
                        used.insert(*next);
                        *next += 1;
                    }
                    let mut preview = false;
                    for tab in &mut g.tabs {
                        if !used.insert(tab.id.0) {
                            tab.id = ViewId(*next);
                            used.insert(*next);
                            *next += 1;
                        }
                        if tab.preview {
                            if preview {
                                tab.preview = false;
                            }
                            preview = true;
                        }
                    }
                    if !g.tabs.iter().any(|t| Some(t.id) == g.active) {
                        g.active = g.tabs.first().map(|t| t.id);
                    }
                }
                SplitNode::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    *ratio = if ratio.is_finite() {
                        ratio.clamp(0.1, 0.9)
                    } else {
                        0.5
                    };
                    repair(first, used, next);
                    repair(second, used, next);
                }
            }
        }
        repair(&mut self.root, &mut used, &mut next);
        self.next_identity = self.next_identity.max(next);
        if !self.groups().iter().any(|g| g.id == self.focused_group) {
            self.focused_group = self.groups()[0].id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(n: u64) -> TranscriptTarget {
        TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(n),
        }
    }
    #[test]
    fn context_tabs_are_reused_per_agent_and_never_stand_in_for_a_transcript() {
        let mut w = Workspace::default();
        let preview = w.open(target(1), false);
        let context = w.open_context(target(1), 8);
        assert_ne!(context, preview, "a context tab must not replace a preview");
        assert_eq!(w.open_context(target(1), 12), context);
        let other = w.open_context(target(2), 12);
        assert_ne!(other, context);

        let tabs = &w.groups()[0].tabs;
        let kinds: Vec<_> = tabs.iter().map(|tab| (tab.kind, tab.preview)).collect();
        assert_eq!(
            kinds,
            [
                (TabKind::Transcript, true),
                (TabKind::Context { turn: 12 }, false),
                (TabKind::Context { turn: 12 }, false),
            ]
        );
        assert_eq!(w.open(target(1), false), preview);
        assert_ne!(w.open(target(2), false), other);

        // Tabs saved before kinds existed restore as transcripts.
        let mut saved = serde_json::to_value(&w.groups()[0].tabs[1]).unwrap();
        saved.as_object_mut().unwrap().remove("kind");
        let restored: Tab = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.kind, TabKind::Transcript);
    }

    #[test]
    fn focus_tab_index_is_local_and_out_of_bounds_is_noop() {
        let mut w = Workspace::default();
        let a = w.open(target(1), true);
        let b = w.open(target(2), true);
        let group = w.split(w.focused_group, SplitAxis::Horizontal).unwrap();
        w.focused_group = group;
        let c = w.open(target(3), true);
        assert!(!w.focus_tab_at(1));
        assert_eq!(w.active_tab().unwrap().id, c);
        assert!(w.focus(a));
        assert!(w.focus_tab_at(1));
        assert_eq!(w.active_tab().unwrap().id, b);
        assert!(!w.focus_tab_at(9));
        assert_eq!(w.active_tab().unwrap().id, b);
    }

    #[test]
    fn directional_focus_uses_geometry_and_can_enter_empty_panes() {
        let mut w = Workspace::default();
        let a = w.open(target(1), true);
        let left = w.focused_group;
        let right_top = w.split(left, SplitAxis::Horizontal).unwrap();
        w.focused_group = right_top;
        let b = w.open(target(2), true);
        let right_bottom = w.split(right_top, SplitAxis::Vertical).unwrap();

        assert!(w.focus_direction(FocusDirection::Down));
        assert_eq!(w.focused_group, right_bottom);
        assert!(w.active_tab().is_none());
        assert!(w.focus_direction(FocusDirection::Left));
        assert_eq!(w.active_tab().unwrap().id, a);
        assert!(!w.focus_direction(FocusDirection::Left));
        assert!(w.focus_direction(FocusDirection::Right));
        assert_eq!(w.active_tab().unwrap().id, b);
        assert!(!w.focus_direction(FocusDirection::Up));
    }

    #[test]
    fn preview_and_global_focus() {
        let mut w = Workspace::default();
        let a = w.open(target(1), false);
        let b = w.open(target(2), false);
        assert_ne!(a, b);
        assert_eq!(w.groups()[0].tabs.len(), 1);
        w.promote(b);
        let g = w.split(w.focused_group, SplitAxis::Horizontal).unwrap();
        w.focused_group = g;
        w.open(target(3), false);
        assert_eq!(w.open(target(2), false), b);
        assert_eq!(w.active_tab().unwrap().id, b);
        assert_eq!(w.groups().len(), 2);
    }
    #[test]
    fn duplicate_mru_and_restore() {
        let mut w = Workspace::default();
        let a = w.open(target(1), true);
        let g = w.split(w.focused_group, SplitAxis::Vertical).unwrap();
        w.focused_group = g;
        let b = w.open_duplicate(target(1), true);
        assert_eq!(w.open(target(1), false), b);
        w.focus(a);
        assert_eq!(w.open(target(1), false), a);
        let mut restored: Workspace =
            serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        restored.restore();
        assert_eq!(restored.active_tab().unwrap().id, a);
        assert_eq!(restored.groups().len(), 2);
        restored.close(a);
        assert_eq!(restored.groups().len(), 1);
    }
    #[test]
    fn replace_target_updates_all_views_without_changing_layout() {
        let mut w = Workspace::default();
        let first = w.open(target(1), true);
        let other = w.open(target(3), true);
        let g = w.split(w.focused_group, SplitAxis::Vertical).unwrap();
        w.focused_group = g;
        let second = w.open_duplicate(target(1), false);
        let before = w.clone();

        w.replace_target(target(1), target(2));
        let tabs: Vec<_> = w.groups().into_iter().flat_map(|g| &g.tabs).collect();
        assert_eq!(
            tabs.iter().find(|t| t.id == first).unwrap().target,
            target(2)
        );
        assert_eq!(
            tabs.iter().find(|t| t.id == second).unwrap().target,
            target(2)
        );
        assert_eq!(
            tabs.iter().find(|t| t.id == other).unwrap().target,
            target(3)
        );
        assert_eq!(w.focused_group, before.focused_group);
        assert_eq!(w.active_tab().unwrap().id, second);
        assert!(w.active_tab().unwrap().preview);
    }
    #[test]
    fn close_and_move_only_remove_their_own_empty_group() {
        let mut w = Workspace::default();
        let a = w.open(target(1), true);
        let first = w.focused_group;
        let empty = w.split(first, SplitAxis::Horizontal).unwrap();
        let destination = w.split(empty, SplitAxis::Vertical).unwrap();
        w.focused_group = empty;
        assert!(w.move_tab(a, destination, 0));
        assert_eq!(w.focused_group, destination);
        assert!(w.groups().iter().any(|g| g.id == empty));
        assert!(!w.groups().iter().any(|g| g.id == first));
        assert!(w.close(a));
        assert_eq!(w.focused_group, empty);
        assert_eq!(w.groups().len(), 1);
        assert!(!w.close_active());
    }

    #[test]
    fn close_empty_pane_and_restore_preserve_other_empty_panes() {
        let mut w = Workspace::default();
        let first = w.focused_group;
        let second = w.split(first, SplitAxis::Horizontal).unwrap();
        let third = w.split(second, SplitAxis::Vertical).unwrap();
        w.focused_group = second;
        w.restore();
        assert_eq!(w.groups().len(), 3);
        assert!(w.close_active());
        assert_eq!(w.focused_group, third);
        assert_eq!(w.groups().len(), 2);
        assert!(w.close_empty_group(first));
        assert_eq!(w.focused_group, third);
        assert!(!w.close_active());
    }

    #[test]
    fn move_to_empty_split_keeps_destination() {
        let mut w = Workspace::default();
        let a = w.open(target(1), false);
        let g = w.split(w.focused_group, SplitAxis::Vertical).unwrap();
        assert!(w.move_tab(a, g, 0));
        assert_eq!(w.groups().len(), 1);
        assert_eq!(w.active_tab().unwrap().id, a);
    }
    #[test]
    fn slots_reorder_within_a_pane_and_insert_into_others() {
        let mut w = Workspace::default();
        let a = w.open(target(1), true);
        let b = w.open(target(2), true);
        let c = w.open(target(3), true);
        let first = w.focused_group;
        let order = |w: &Workspace, group: GroupId| -> Vec<ViewId> {
            w.groups()
                .iter()
                .find(|g| g.id == group)
                .unwrap()
                .tabs
                .iter()
                .map(|t| t.id)
                .collect()
        };
        assert!(w.move_tab_to_slot(a, first, 3));
        assert_eq!(order(&w, first), [b, c, a]);
        assert!(w.move_tab_to_slot(a, first, 0));
        assert_eq!(order(&w, first), [a, b, c]);
        assert!(w.move_tab_to_slot(c, first, 1));
        assert_eq!(order(&w, first), [a, c, b]);
        let second = w.split(first, SplitAxis::Horizontal).unwrap();
        assert!(w.move_tab_to_slot(b, second, 0));
        assert_eq!(order(&w, second), [b]);
        assert!(w.move_tab_to_slot(a, second, 1));
        assert_eq!(order(&w, second), [b, a]);
        assert_eq!(order(&w, first), [c]);
    }
}
