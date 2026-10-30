use psa_api::{AgentId, SessionId};
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: ViewId,
    pub target: TranscriptTarget,
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
    fn prune(self) -> Self {
        match self {
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = first.prune();
                let second = second.prune();
                if matches!(&first, Self::Group(g) if g.tabs.is_empty()) {
                    return second;
                }
                if matches!(&second, Self::Group(g) if g.tabs.is_empty()) {
                    return first;
                }
                Self::Split {
                    axis,
                    ratio: if ratio.is_finite() {
                        ratio.clamp(0.1, 0.9)
                    } else {
                        0.5
                    },
                    first: Box::new(first),
                    second: Box::new(second),
                }
            }
            other => other,
        }
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
            .filter(|t| t.target == target)
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
    /// Internal/future duplicate support; ordinary navigation must use `open`.
    pub fn open_duplicate(&mut self, target: TranscriptTarget, permanent: bool) -> ViewId {
        let id = ViewId(self.next_id());
        let clock = self.next_focus();
        if self.root.group_mut(self.focused_group).is_none() {
            self.focused_group = self.groups()[0].id;
        }
        let group = self.root.group_mut(self.focused_group).unwrap();
        let tab = Tab {
            id,
            target,
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
        self.prune_empty_groups();
        true
    }
    fn prune_empty_groups(&mut self) {
        self.root = self.root.clone().prune();
        if !self.groups().iter().any(|g| g.id == self.focused_group) {
            self.focused_group = self.groups()[0].id;
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
        self.prune_empty_groups();
        true
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
        self.prune_empty_groups();
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
    fn move_to_empty_split_keeps_destination() {
        let mut w = Workspace::default();
        let a = w.open(target(1), false);
        let g = w.split(w.focused_group, SplitAxis::Vertical).unwrap();
        assert!(w.move_tab(a, g, 0));
        assert_eq!(w.groups().len(), 1);
        assert_eq!(w.active_tab().unwrap().id, a);
    }
}
