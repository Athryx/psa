//! Session list filter menu: probe and project checkmarks with an option search.
//!
//! An empty selection within a section matches everything, mirroring
//! [`SessionFilter`]. Sandboxes are listed under their manager, and toggling a
//! manager toggles its sandboxes with it.
use crate::{
    app::Ui,
    network,
    reactive::use_ui_subscription,
    views::{probe_label, probe_status_class},
};
use dioxus::prelude::*;
use psa_api::{ProbeId, ProjectId, SessionFilter};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProbeOption {
    pub(crate) id: ProbeId,
    pub(crate) label: String,
    pub(crate) status: &'static str,
}

/// A top-level probe and the sandboxes it manages.
#[derive(Clone, PartialEq)]
pub(crate) struct ProbeGroup {
    pub(crate) manager: ProbeOption,
    pub(crate) sandboxes: Vec<ProbeOption>,
}

impl ProbeGroup {
    pub(crate) fn options(&self) -> impl Iterator<Item = &ProbeOption> {
        std::iter::once(&self.manager).chain(&self.sandboxes)
    }

    fn ids(&self) -> impl Iterator<Item = ProbeId> + '_ {
        std::iter::once(self.manager.id).chain(self.sandboxes.iter().map(|s| s.id))
    }

    /// The group restricted to options matching `search`; a matching manager keeps
    /// all of its sandboxes.
    pub(crate) fn matching(&self, search: &str) -> Option<Self> {
        if matches_search(&self.manager.label, search) {
            return Some(self.clone());
        }
        let sandboxes: Vec<_> = self
            .sandboxes
            .iter()
            .filter(|sandbox| matches_search(&sandbox.label, search))
            .cloned()
            .collect();
        (!sandboxes.is_empty()).then(|| Self {
            manager: self.manager.clone(),
            sandboxes,
        })
    }
}

fn matches_search(label: &str, search: &str) -> bool {
    label.to_lowercase().contains(&search.to_lowercase())
}

pub(crate) fn build_probe_option(state: &Ui, id: ProbeId, fallback: &str) -> ProbeOption {
    ProbeOption {
        id,
        label: state
            .probes
            .iter()
            .find(|p| p.metadata.id == id)
            .map(probe_label)
            .unwrap_or_else(|| fallback.to_owned()),
        status: probe_status_class(state, id),
    }
}

/// Connected probes, with each sandbox nested under its manager when the manager
/// is connected too; otherwise the sandbox is listed on its own.
pub(crate) fn build_probe_groups(state: &Ui) -> Vec<ProbeGroup> {
    let has_probe = |id: ProbeId| state.probes.iter().any(|p| p.metadata.id == id);
    let is_nested = |id: ProbeId| {
        state
            .sandboxes
            .iter()
            .any(|s| s.probe_id == id && has_probe(s.manager_id))
    };
    state
        .probes
        .iter()
        .filter(|probe| !is_nested(probe.metadata.id))
        .map(|probe| ProbeGroup {
            manager: build_probe_option(state, probe.metadata.id, "Unknown probe"),
            sandboxes: state
                .sandboxes
                .iter()
                .filter(|sandbox| sandbox.manager_id == probe.metadata.id)
                .map(|sandbox| build_probe_option(state, sandbox.probe_id, &sandbox.name))
                .collect(),
        })
        .collect()
}

/// Projects known from the server list and from loaded sessions, sorted.
fn build_projects(state: &Ui) -> Vec<ProjectId> {
    let mut projects: Vec<_> = state
        .projects
        .iter()
        .cloned()
        .chain(state.sessions.iter().filter_map(|s| s.project_id.clone()))
        .collect();
    projects.sort();
    projects.dedup();
    projects
}

fn toggle<T: PartialEq>(list: &mut Vec<T>, item: T) {
    match list.iter().position(|existing| *existing == item) {
        Some(index) => {
            list.remove(index);
        }
        None => list.push(item),
    }
}

/// Select or deselect a manager together with every sandbox it manages.
fn toggle_probe_group(filter: &mut SessionFilter, group: &ProbeGroup) {
    let ids: Vec<_> = group.ids().collect();
    if filter.probe_ids.contains(&group.manager.id) {
        filter.probe_ids.retain(|id| !ids.contains(id));
        return;
    }
    for id in ids {
        if !filter.probe_ids.contains(&id) {
            filter.probe_ids.push(id);
        }
    }
}

/// Apply a selection change and reload the first page under the new filter.
fn change_filter(
    mut ui: Signal<Ui>,
    onchange: EventHandler<()>,
    update: impl FnOnce(&mut SessionFilter),
) {
    let mut state = ui.write();
    update(&mut state.session_filter);
    // Cursors are scoped to the filter that produced them.
    state.next_sessions = None;
    drop(state);
    onchange.call(());
    spawn(network::load_sessions(ui, false));
}

pub(crate) fn check_mark(checked: bool, mixed: bool) -> &'static str {
    match (checked, mixed) {
        (true, _) => "✓",
        (false, true) => "–",
        (false, false) => "",
    }
}

#[component]
pub(crate) fn SessionFilterMenu(onchange: EventHandler<()>) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut open = use_signal(|| false);
    let mut search = use_signal(String::new);
    use_ui_subscription(|state| {
        (
            state.session_filter.probe_ids.clone(),
            state.session_filter.project_ids.clone(),
        )
    });
    let selected = {
        let state = ui.peek();
        state.session_filter.probe_ids.len() + state.session_filter.project_ids.len()
    };
    let label = if selected == 0 {
        "Filter".to_owned()
    } else {
        format!("Filter · {selected}")
    };

    rsx! {
        div { class: "filter-menu-anchor",
            button { r#type: "button", class: if selected > 0 { "filter-button active" } else { "filter-button" },
                title: "Filter sessions by probe and project", "aria-haspopup": "dialog", "aria-expanded": open(),
                onclick: move |_| {
                    search.set(String::new());
                    open.set(true);
                    spawn(network::refresh_projects(ui));
                },
                "{label}"
            }
            if open() {
                div { class: "row-menu-backdrop", onclick: move |_| open.set(false), oncontextmenu: move |event| { event.prevent_default(); open.set(false); } }
                FilterMenu { search, onchange, onclose: move |_| open.set(false) }
            }
        }
    }
}

/// Mounted only while open, so its option lists reflect current probes and projects.
#[component]
fn FilterMenu(
    search: Signal<String>,
    onchange: EventHandler<()>,
    onclose: EventHandler<()>,
) -> Element {
    let ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| {
        (
            state.session_filter.probe_ids.clone(),
            state.session_filter.project_ids.clone(),
            state.probes.clone(),
            state.sandboxes.clone(),
            state.projects.clone(),
        )
    });
    let state = ui.peek();
    let query = search();
    let groups: Vec<_> = build_probe_groups(&state)
        .iter()
        .filter_map(|group| group.matching(&query))
        .collect();
    let projects: Vec<_> = build_projects(&state)
        .into_iter()
        .filter(|project| matches_search(&project.0, &query))
        .collect();
    let show_global = matches_search("Global", &query);
    let no_projects = !show_global && projects.is_empty();
    let probe_ids = state.session_filter.probe_ids.clone();
    let project_ids = state.session_filter.project_ids.clone();
    drop(state);
    let has_probe_selection = !probe_ids.is_empty();
    let has_project_selection = !project_ids.is_empty();
    let has_selection = has_probe_selection || has_project_selection;

    rsx! {
        div { class: "filter-menu", role: "dialog", "aria-label": "Session filters",
            onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); onclose.call(()); } },
            input { r#type: "search", class: "filter-menu-search", placeholder: "Find probes and projects", "aria-label": "Find probes and projects",
                value: "{query}", oninput: move |event| search.set(event.value()),
                onmounted: move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
            }
            div { class: "filter-section",
                div { class: "filter-section-title", "Probes" }
                FilterOption { label: "All", status: "", checked: probe_ids.is_empty(), mixed: false, nested: false,
                    ontoggle: move |_| if has_probe_selection { change_filter(ui, onchange, |filter| filter.probe_ids.clear()) } }
                if groups.is_empty() { p { class: "muted filter-empty", "No matching probes" } }
                for group in groups {
                    {
                        let checked = probe_ids.contains(&group.manager.id);
                        let mixed = group.sandboxes.iter().any(|s| probe_ids.contains(&s.id));
                        let toggled = group.clone();
                        rsx! {
                            FilterOption { key: "{group.manager.id}", label: group.manager.label.clone(), status: group.manager.status, checked, mixed, nested: false,
                                ontoggle: move |_| { let group = toggled.clone(); change_filter(ui, onchange, move |filter| toggle_probe_group(filter, &group)); } }
                            for sandbox in group.sandboxes {
                                FilterOption { key: "{sandbox.id}", label: sandbox.label, status: sandbox.status, checked: probe_ids.contains(&sandbox.id), mixed: false, nested: true,
                                    ontoggle: move |_| change_filter(ui, onchange, move |filter| toggle(&mut filter.probe_ids, sandbox.id)) }
                            }
                        }
                    }
                }
            }
            div { class: "filter-section",
                div { class: "filter-section-title", "Projects" }
                FilterOption { label: "All", status: "", checked: project_ids.is_empty(), mixed: false, nested: false,
                    ontoggle: move |_| if has_project_selection { change_filter(ui, onchange, |filter| filter.project_ids.clear()) } }
                if show_global {
                    FilterOption { label: "Global", status: "", checked: project_ids.contains(&None), mixed: false, nested: false,
                        ontoggle: move |_| change_filter(ui, onchange, |filter| toggle(&mut filter.project_ids, None)) }
                }
                for project in projects {
                    {
                        let checked = project_ids.contains(&Some(project.clone()));
                        let toggled = project.clone();
                        rsx! {
                            FilterOption { key: "{project}", label: project.0.clone(), status: "", checked, mixed: false, nested: false,
                                ontoggle: move |_| { let project = toggled.clone(); change_filter(ui, onchange, move |filter| toggle(&mut filter.project_ids, Some(project))); } }
                        }
                    }
                }
                if no_projects { p { class: "muted filter-empty", "No matching projects" } }
            }
            div { class: "filter-menu-footer",
                button { r#type: "button", disabled: !has_selection,
                    onclick: move |_| change_filter(ui, onchange, |filter| { filter.probe_ids.clear(); filter.project_ids.clear(); }),
                    "Clear selection"
                }
            }
        }
    }
}

/// One checkable row; `status` names a `.probe-dot` modifier, or is empty for no dot.
#[component]
fn FilterOption(
    label: String,
    status: &'static str,
    checked: bool,
    mixed: bool,
    nested: bool,
    ontoggle: EventHandler<()>,
) -> Element {
    let aria_checked = match (checked, mixed) {
        (true, _) => "true",
        (false, true) => "mixed",
        (false, false) => "false",
    };
    rsx! {
        button { r#type: "button", role: "menuitemcheckbox", "aria-checked": aria_checked,
            class: if nested { "filter-option nested" } else { "filter-option" },
            title: "{label}",
            onclick: move |_| ontoggle.call(()),
            span { class: "filter-check", "aria-hidden": "true", "{check_mark(checked, mixed)}" }
            if !status.is_empty() { span { class: "probe-dot {status}", "aria-hidden": "true" } }
            span { class: "filter-option-label", "{label}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> ProbeId {
        ProbeId(psa_api::probes::Uuid::from_u128(n))
    }

    fn option(n: u128, label: &str) -> ProbeOption {
        ProbeOption {
            id: id(n),
            label: label.into(),
            status: "online",
        }
    }

    fn group() -> ProbeGroup {
        ProbeGroup {
            manager: option(1, "Host"),
            sandboxes: vec![option(2, "alpha"), option(3, "beta")],
        }
    }

    #[test]
    fn manager_toggles_its_sandboxes_and_sandboxes_toggle_alone() {
        let mut filter = SessionFilter::default();
        toggle_probe_group(&mut filter, &group());
        assert_eq!(filter.probe_ids, vec![id(1), id(2), id(3)]);

        toggle(&mut filter.probe_ids, id(2));
        assert_eq!(filter.probe_ids, vec![id(1), id(3)]);

        toggle_probe_group(&mut filter, &group());
        assert!(filter.probe_ids.is_empty());

        toggle(&mut filter.probe_ids, id(3));
        toggle_probe_group(&mut filter, &group());
        assert_eq!(filter.probe_ids, vec![id(3), id(1), id(2)]);
    }

    #[test]
    fn search_keeps_matching_sandboxes_or_the_whole_matching_manager() {
        let only_beta = group().matching("BET").unwrap();
        assert_eq!(only_beta.manager.id, id(1));
        assert_eq!(only_beta.sandboxes, vec![option(3, "beta")]);
        assert_eq!(group().matching("host").unwrap().sandboxes.len(), 2);
        assert!(group().matching("missing").is_none());
    }
}
