use crate::{app::Ui, network, reactive::use_ui_subscription};
use client_shared::*;
use dioxus::prelude::*;
use llm_api::providers::ProviderType;

#[cfg(test)]
pub(crate) use crate::transcript_view::EventRow;
use crate::transcript_view::TranscriptView;
use psa_api::*;

#[cfg(test)]
#[path = "tab_spinner_tests.rs"]
mod tab_spinner_tests;

/// DOM id the sidebar's edge resizer sizes in the browser during a drag.
pub(crate) const SIDEBAR_ID: &str = "psa-sidebar";

#[component]
pub fn Rail() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| state.sidebar.clone());
    rsx! {nav{class:"rail","aria-label":"Workspace navigation",
        button{title:"Session browser","aria-label":"Session browser","aria-pressed":!ui.peek().sidebar.collapsed && ui.peek().sidebar.expanded_mode == SidebarMode::Browser,onclick:move |_|ui.write().sidebar.toggle_mode(SidebarMode::Browser),"▤"}
        button{title:"Current session and agents","aria-label":"Current session and agents","aria-pressed":!ui.peek().sidebar.collapsed && ui.peek().sidebar.expanded_mode == SidebarMode::Details,onclick:move |_|ui.write().sidebar.toggle_mode(SidebarMode::Details),"♧"}
        div{class:"spacer"}
        button{title:"Settings",onclick:move |_|ui.write().settings_open=true,"⚙"}
    }}
}

#[component]
pub fn Sidebar() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut frozen_order = use_signal(|| None::<Vec<SessionId>>);
    use_ui_subscription(|state| {
        (
            state.sidebar.clone(),
            state.selected_probe,
            state.sessions.clone(),
            state.fresh_sessions.clone(),
            state.probes.clone(),
            state.connection.clone(),
            state.next_sessions.is_some(),
            state.generation,
            state.creation_target.clone(),
            state.creation_revision,
            state.creation_pending,
        )
    });
    let state = ui.peek();
    if state.sidebar.expanded_mode == SidebarMode::Details {
        return rsx! {aside{id:SIDEBAR_ID,class:"sidebar",style:"width:{state.sidebar.width()}px",Details{}}};
    }
    let probe = state.selected_probe;
    let mut index = SessionIndex::default();
    for session in &state.sessions {
        if state.connection != "Connected" || state.fresh_sessions.contains(&session.id) {
            index.upsert(session.clone());
        }
    }
    index.fully_loaded = state.next_sessions.is_none();
    let mut groups = index.groups(probe.as_ref());
    if let Some(order) = frozen_order.read().as_ref() {
        let ranks: std::collections::HashMap<_, _> = order
            .iter()
            .enumerate()
            .map(|(rank, id)| (*id, rank))
            .collect();
        for group in &mut groups {
            group
                .sessions
                .sort_by_key(|session| ranks.get(&session.id).copied().unwrap_or(usize::MAX));
        }
        groups.sort_by_key(|group| {
            group
                .sessions
                .iter()
                .filter_map(|session| ranks.get(&session.id))
                .min()
                .copied()
                .unwrap_or(usize::MAX)
        });
    }
    let creation_target = state.creation_target.clone();
    let target_in_groups = creation_target.as_ref().is_some_and(|target| {
        groups.iter().any(|group| {
            group.probe_id == target.probe
                && group.working_directory.as_os_str() == std::ffi::OsStr::new(&target.directory)
        })
    });
    let displayed_order: Vec<_> = groups
        .iter()
        .flat_map(|group| group.sessions.iter().map(|session| session.id))
        .collect();
    let probe_options: Vec<_> = state
        .probes
        .iter()
        .map(|p| {
            (
                p.metadata.id,
                format!(
                    "{} · {}",
                    probe_label(p),
                    if p.is_active { "online" } else { "offline" }
                ),
            )
        })
        .collect();
    rsx! {aside{id:SIDEBAR_ID,class:"sidebar",style:"width:{state.sidebar.width()}px",
        onmouseenter: move |_| frozen_order.set(Some(displayed_order.clone())),
        onmouseleave: move |_| frozen_order.set(None),
        div{class:"sessions-heading",
            h2{"Sessions"}
            crate::creation::CreateSessionPicker { key: "{state.generation}" }
        }
        label{class:"sr-only",r#for:"probe-filter","Probe"}
        select{id:"probe-filter",value:probe.map(|p|p.to_string()).unwrap_or_default(),onchange:move |event|{let id=ui.read().probes.iter().find(|p|p.metadata.id.to_string()==event.value()).map(|p|p.metadata.id);ui.write().selected_probe=id;ui.write().next_sessions=None;frozen_order.set(None);spawn(network::load_sessions(ui,false));},
            option{value:"","All probes"}
            for (id,label) in probe_options{option{value:"{id}","{label}"}}
        }
        if let Some(id) = probe { div { class: "folder-probe", ProbeName { id } } }
        if groups.is_empty(){p{class:"muted","No sessions loaded. Check the connection or choose another probe."}}
        for group in groups{
            div{key:"{group.probe_id}:{group.working_directory.display()}",class:"folder",
                details{key: if creation_target.as_ref().is_some_and(|target| target.probe == group.probe_id && group.working_directory.as_os_str() == std::ffi::OsStr::new(&target.directory)) { state.creation_revision.to_string() } else { "0".to_string() },open:true,
                    summary{"{group.working_directory.display()}" if group.busy{span{class:"tab-spinner inline-spinner",role:"img","aria-label":"Session running"}}}
                    if probe.is_none() { div { class: "folder-probe", ProbeName { id: group.probe_id } } }
                    for session in group.sessions{
                        button{key:"{session.id.0}",class:"session",title:"{session.name}",
                            onclick:move |_|{spawn(network::open_session(ui,session.id,false));},
                            ondoubleclick:move |_|{spawn(network::double_click_session(ui,session.id));},
                            if session.parent_id.is_some(){"⑂ "}
                            "{session.name}"
                            if session.busy{span{class:"tab-spinner inline-spinner",role:"img","aria-label":"Session running"}}else if state.connection!="Connected"{span{class:"muted"," ?"}}
                        }
                    }
                    if let Some(target) = creation_target.as_ref().filter(|target| target.probe == group.probe_id && group.working_directory.as_os_str() == std::ffi::OsStr::new(&target.directory)) {
                        crate::creation::InlineSessionName { target: target.clone() }
                    }
                }
                button { class: "folder-add", title: "New session in this folder", "aria-label": "New session in this folder", disabled: state.connection != "Connected" || state.creation_pending,
                    onclick: move |event| { event.prevent_default(); event.stop_propagation(); crate::creation::select_target(&mut ui.write(), crate::creation::CreationTarget { probe: group.probe_id, directory: group.working_directory.to_string_lossy().into_owned() }); }, "+"
                }
            }
        }
        if let Some(target) = creation_target.filter(|_| !target_in_groups) {
            div { class: "folder transient-folder", div { class: "transient-folder-title", "{target.directory} · " ProbeName { id: target.probe } } crate::creation::InlineSessionName { target } }
        }
        if state.next_sessions.is_some(){button{onclick:move |_|{spawn(network::more_sessions(ui));},"Load more sessions"}p{class:"muted","Folder groups may be incomplete until all pages are loaded."}}
    }}
}

pub(crate) fn probe_label(probe: &Probe) -> String {
    probe
        .metadata
        .label
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&probe.metadata.hostname)
        .to_owned()
}

#[cfg(test)]
mod probe_name_tests {
    use super::*;

    #[test]
    fn label_prefers_nonblank_label_then_hostname() {
        let mut probe: Probe = serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "hostname": "host", "os": "linux", "architecture": "x86_64",
            "label": "Workstation", "data_dir": "/data", "host": false,
            "is_active": true, "created_at": "2026-01-01T00:00:00Z",
            "last_connected_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(probe_label(&probe), "Workstation");
        probe.metadata.label = Some("  ".into());
        assert_eq!(probe_label(&probe), "host");
        probe.metadata.label = None;
        assert_eq!(probe_label(&probe), "host");
    }
}

#[component]
pub(crate) fn ProbeName(id: ProbeId) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut open = use_signal(|| false);
    let mut trigger = use_signal(|| None::<std::rc::Rc<MountedData>>);
    crate::app::register_modal(open);
    use_ui_subscription(|state| state.probes.clone());
    let probe = ui
        .peek()
        .probes
        .iter()
        .find(|p| p.metadata.id == id)
        .cloned();
    let name = probe
        .as_ref()
        .map(probe_label)
        .unwrap_or_else(|| "Unknown probe".into());
    rsx! {
        button { class: "probe-name", title: "Probe information", onmounted: move |event| trigger.set(Some(event.data())), onclick: move |event| { event.stop_propagation(); open.set(true); }, "{name}" }
        if open() {
            div { class: "modal-backdrop", onclick: move |event| { event.stop_propagation(); open.set(false); if let Some(node) = trigger() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                section { class: "settings agent-info", role: "dialog", "aria-modal": "true", "aria-label": "Probe information", tabindex: "-1",
                    onclick: move |event| event.stop_propagation(),
                    onmounted: move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); open.set(false); if let Some(node) = trigger() { spawn(async move { let _ = node.set_focus(true).await; }); } } },
                    crate::modal::ModalHeader { title: "Probe information", close_label: "Close probe information", onclose: move |_| { open.set(false); if let Some(node) = trigger() { spawn(async move { let _ = node.set_focus(true).await; }); } } }
                    dl { class: "info-grid",
                        dt { "Name" } dd { "{name}" }
                        dt { "UUID" } dd { "{id}" }
                        if let Some(probe) = probe {
                            dt { "Status" } dd { if probe.is_active { "Online" } else { "Offline" } }
                            dt { "Hostname" } dd { "{probe.metadata.hostname}" }
                            dt { "Operating system" } dd { "{probe.metadata.os}" }
                            dt { "Architecture" } dd { "{probe.metadata.architecture}" }
                            dt { "Data directory" } dd { "{probe.metadata.data_dir.display()}" }
                            dt { "Home directory" } dd { "{probe.metadata.home_dir.as_deref().unwrap_or(\"Unknown\")}" }
                            dt { "Host probe" } dd { if probe.metadata.host { "Yes" } else { "No" } }
                            dt { "First registered" } dd { "{probe.created_at}" }
                            dt { "Last connected" } dd { "{probe.last_connected_at}" }
                        } else { dt { "Status" } dd { "Unknown (metadata unavailable)" } }
                    }
                }
            }
        }
    }
}

#[component]
fn Details() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| {
        (
            state.workspace.active_tab().map(|tab| tab.target),
            state.sessions.clone(),
            state.agents.clone(),
            state.settings.preferences.show_hidden_agents,
            state.models.clone(),
        )
    });
    let state = ui.peek();
    let Some(tab) = state.workspace.active_tab() else {
        return rsx! {h2{"Current session"}p{class:"muted","Select a session or agent tab."}};
    };
    let target = tab.target;
    let Some(session) = state.sessions.iter().find(|s| s.id == target.session_id) else {
        return rsx! {h2{"Session {target.session_id.0}"}p{class:"muted","Restored tab; waiting for session metadata."}};
    };
    let agents = state
        .agents
        .get(&target.session_id)
        .cloned()
        .unwrap_or_default();
    let tree = agent_tree(&agents, state.settings.preferences.show_hidden_agents);
    // A fork's parent may be outside the loaded session list.
    let parent = session.parent_id.map(|id| {
        let name = state
            .sessions
            .iter()
            .find(|candidate| candidate.id == id)
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| format!("Session {}", id.0));
        (id, name)
    });
    rsx! {
        div{class:"sessions-heading", h2{"Current session"}}
        strong{class:"details-session-name","{session.name}"}
        dl{class:"details-grid",
            if let Some((parent, parent_name)) = parent {
                dt{"Forked from"}
                dd{
                    button{class:"text-link",title:"Open parent session",
                        onclick:move |_|{spawn(network::open_session_by_id(ui,parent));},
                        "{parent_name}"
                    }
                }
            }
            dt{"Working directory"}dd{"{session.working_directory.display()}"}
            dt{"Probe"}dd{ProbeName{id:session.probe_id}}
            dt{"Default model"}
            dd{title:"{session.model_config.model.provider.as_str()}/{session.model_config.model.api_name}",
                crate::composer::ModelEffort{
                    model:crate::composer::model_display_name(&state.models,&session.model_config.model),
                    effort:session.model_config.effort,
                }
            }
        }
        section{class:"details-agents",
            div{class:"details-agents-heading",
                h2{"Agents"}
                button{class:"details-toggle", "aria-pressed":state.settings.preferences.show_hidden_agents,
                    onclick:move |_|{let show=ui.read().settings.preferences.show_hidden_agents;ui.write().settings.preferences.show_hidden_agents= !show;},
                    if state.settings.preferences.show_hidden_agents{"Hide internal"}else{"Show internal"}
                }
            }
            for row in tree{
                button{class:"agent",style:"padding-left:{8+row.depth*14}px",disabled:row.context_only,
                    onclick:move |_|ui.write().open(TranscriptTarget{session_id:target.session_id,agent_id:row.agent.id},false),
                    span{class:"agent-name","{row.agent.name}" if row.context_only{" (hidden ancestor)"}}
                    if row.agent.state == AgentState::Running { span{class:"tab-spinner",role:"img","aria-label":"Agent running"} }
                }
            }
        }
    }
}

#[component]
pub fn Layout(node: SplitNode, path: Vec<bool>) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let split_id = format!(
        "psa-split-{}",
        path.iter()
            .map(|side| if *side { '1' } else { '0' })
            .collect::<String>()
    );
    match node {
        SplitNode::Group(group) => rsx! {Pane{group}},
        SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let vertical = axis == SplitAxis::Vertical;
            let mut first_path = path.clone();
            first_path.push(false);
            let mut second_path = path.clone();
            second_path.push(true);
            rsx! {div{id:split_id.clone(),class:if vertical{"split vertical"}else{"split"},
                div{class:"branch",style:"flex:{ratio};",Layout{node:*first,path:first_path}}
                crate::interaction::SplitDivider {
                    split_id:split_id.clone(), path: path.clone(), axis, ratio: f64::from(ratio),
                    oncommit: move |value| set_ratio(&mut ui.write().workspace.root, &path, value as f32),
                }
                div{class:"branch",style:"flex:{1.0-ratio};",Layout{node:*second,path:second_path}}
            }}
        }
    }
}

#[cfg(test)]
pub(crate) fn drag_ratio(coordinate: f64, origin: f64, size: f64) -> Option<f32> {
    if !coordinate.is_finite() || !origin.is_finite() || !size.is_finite() || size <= 0.0 {
        return None;
    }
    Some(((coordinate - origin) / size).clamp(0.1, 0.9) as f32)
}
fn set_ratio(node: &mut SplitNode, path: &[bool], ratio: f32) {
    if let SplitNode::Split {
        ratio: current,
        first,
        second,
        ..
    } = node
    {
        if let Some((side, tail)) = path.split_first() {
            set_ratio(if *side { second } else { first }, tail, ratio);
        } else {
            *current = ratio.clamp(0.1, 0.9);
        }
    }
}

#[component]
fn Pane(group: TabGroup) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| {
        (
            state.workspace.focused_group,
            state.sessions.clone(),
            state.agents.clone(),
            state.connection.clone(),
            state.transcript_status.clone(),
        )
    });
    let state = ui.peek();
    let focused = state.workspace.focused_group == group.id;
    let active = group
        .tabs
        .iter()
        .find(|t| Some(t.id) == group.active)
        .cloned();
    rsx! {section{class:if focused{"pane focused"}else{"pane"}, "data-pane-id":"{group.id.0}",
        onmousedown:move |_|{if ui.peek().workspace.focused_group != group.id { ui.write().workspace.focused_group=group.id; }},
        onfocusin:move |_|{if ui.peek().workspace.focused_group != group.id { ui.write().workspace.focused_group=group.id; }},
        div{class:"tabs",
            for tab in group.tabs.iter().cloned(){
                // Dragging is handled in the browser by tab_drag.js.
                div{key:"{tab.id.0}",class:format!("tab {} {}",if Some(tab.id)==group.active{"active"}else{""},if tab.preview{"preview"}else{""}),"data-tab-id":"{tab.id.0}",
                    button{class:"title",onclick:move |_|{ui.write().workspace.focus(tab.id);if tab.target.agent_id != crate::app::DRAFT_ROOT { ui.write().last_agents.insert(tab.target.session_id,tab.target.agent_id); } crate::app::request_composer_focus(ui);},ondoubleclick:move |_|ui.write().workspace.promote(tab.id),
                        "{tab_title(&state,tab.target)}" if matches!(tab.kind, TabKind::Context { .. }) { " · context" }
                    }
                    if tab_is_running(&state, tab.target) { span { class:"tab-spinner", role:"img", "aria-label":"Agent running" } }
                    button{class:"close",title:"Close tab",onclick:move |_|{ui.write().workspace.close(tab.id);},"×"}
                }
            }
            div{class:"tab-actions",
                button{title:"Split horizontally","aria-label":"Split horizontally",onclick:move |_|{ui.write().workspace.split(group.id,SplitAxis::Horizontal);},span{class:"split-icon","aria-hidden":"true"}}
                button{title:"Split vertically","aria-label":"Split vertically",onclick:move |_|{ui.write().workspace.split(group.id,SplitAxis::Vertical);},span{class:"split-icon split-icon-vertical","aria-hidden":"true"}}
            }
        }
        for tab in group.tabs.iter().cloned(){div{key:"{tab.id.0}","data-view-id":"{tab.id.0}","data-active":if Some(tab.id)==group.active{"true"}else{"false"},style:if Some(tab.id)==group.active{"display:flex;flex-direction:column;flex:1;min-height:0;"}else{"display:none;"},
            match tab.kind {
                TabKind::Transcript => rsx!{TranscriptView{key:"{tab.id.0}:{tab.target.session_id.0}:{tab.target.agent_id.0}",active:Some(tab.id)==group.active,tab:tab.clone()}},
                TabKind::Context { turn } => rsx!{crate::context_view::ContextView{key:"{tab.id.0}:{tab.target.session_id.0}:{tab.target.agent_id.0}",target:tab.target,turn}},
            }
        }}
        if active.is_none(){div{class:"empty","Select an agent to open a preview here."}}
    }}
}
/// Edge of a pane that a dropped tab splits off into a new pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// A completed tab drag reported by tab_drag.js.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TabDrop {
    /// Dropped on a tab bar, between existing tabs.
    Insert { view: u64, pane: u64, slot: usize },
    /// Dropped on another pane's body: appended to its tab bar.
    Join { view: u64, pane: u64 },
    /// Dropped near a pane's edge: split into a new pane on that side.
    Split {
        view: u64,
        pane: u64,
        edge: DropEdge,
    },
}

pub(crate) fn apply_tab_drop(workspace: &mut Workspace, drop: TabDrop) {
    match drop {
        TabDrop::Insert { view, pane, slot } => {
            workspace.move_tab_to_slot(ViewId(view), GroupId(pane), slot);
        }
        TabDrop::Join { view, pane } => {
            workspace.move_tab(ViewId(view), GroupId(pane), usize::MAX);
        }
        TabDrop::Split { view, pane, edge } => {
            let (axis, before) = match edge {
                DropEdge::Left => (SplitAxis::Horizontal, true),
                DropEdge::Right => (SplitAxis::Horizontal, false),
                DropEdge::Top => (SplitAxis::Vertical, true),
                DropEdge::Bottom => (SplitAxis::Vertical, false),
            };
            move_to_split(workspace, ViewId(view), GroupId(pane), axis, before);
        }
    }
}

pub(crate) fn move_to_split(
    workspace: &mut Workspace,
    view: ViewId,
    group: GroupId,
    axis: SplitAxis,
    before: bool,
) {
    if !workspace
        .groups()
        .iter()
        .any(|group| group.tabs.iter().any(|tab| tab.id == view))
    {
        return;
    }
    let Some(new) = workspace.split(group, axis) else {
        return;
    };
    if before {
        fn place_first(node: &mut SplitNode, new: GroupId) -> bool {
            let SplitNode::Split { first, second, .. } = node else {
                return false;
            };
            if matches!(second.as_ref(), SplitNode::Group(group) if group.id == new) {
                std::mem::swap(first, second);
                return true;
            }
            place_first(first, new) || place_first(second, new)
        }
        place_first(&mut workspace.root, new);
    }
    workspace.move_tab(view, new, 0);
}

fn tab_is_running(state: &Ui, target: TranscriptTarget) -> bool {
    target.agent_id != crate::app::DRAFT_ROOT
        && state.connection == "Connected"
        && state
            .transcript_status
            .get(&target)
            .is_some_and(|status| status == "Live")
        && state.agents.get(&target.session_id).is_some_and(|agents| {
            agents
                .iter()
                .any(|agent| agent.id == target.agent_id && agent.state == AgentState::Running)
        })
}

fn tab_title(state: &Ui, target: TranscriptTarget) -> String {
    let session = state
        .sessions
        .iter()
        .find(|s| s.id == target.session_id)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| format!("Session {}", target.session_id.0));
    if target.agent_id == crate::app::DRAFT_ROOT {
        return format!("{session} · Root");
    }
    let agent = state
        .agents
        .get(&target.session_id)
        .and_then(|a| a.iter().find(|a| a.id == target.agent_id))
        .map(|a| a.name.clone())
        .unwrap_or_else(|| format!("Agent {}", target.agent_id.0));
    format!("{session} · {agent}")
}

#[component]
pub fn Settings() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut tab = use_signal(|| "profiles".to_string());
    use_ui_subscription(|state| {
        (
            state.settings.clone(),
            state.models.clone(),
            state.error.clone(),
            state.generation,
        )
    });
    let state = ui.peek();
    let profile_id = state.settings.active_profile_id.clone();
    let mut filter = use_signal(String::new);
    let needle = filter().to_lowercase();
    let mut models: Vec<_> = if tab() == "models" {
        state
            .models
            .iter()
            .filter(|model| {
                model.name.to_lowercase().contains(&needle)
                    || model.model.api_name.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    models.sort_by_key(|model| !model.pinned);
    let favorite_count = models.iter().take_while(|model| model.pinned).count();
    let show_divider = favorite_count > 0 && favorite_count < models.len();
    rsx! {div{class:"modal-backdrop",onclick:move |event|{event.stop_propagation();ui.write().settings_open=false;},section{class:"settings settings-dialog",role:"dialog","aria-modal":"true","aria-label":"Settings",tabindex:"-1",onclick:move |event|event.stop_propagation(),
        onmounted:move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
        onkeydown:move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); ui.write().settings_open=false; } },
        crate::modal::ModalHeader{title:"Settings",close_label:"Close settings",onclose:move |_|ui.write().settings_open=false}
        if let Some(error) = &state.error {
            div { class:"settings-error error", role:"alert", "{error}" button { onclick:move |_|ui.write().error=None, "Dismiss" } }
        }
        div { class:"settings-layout",
        nav { class:"settings-nav", "aria-label":"Settings sections",
            button { class:if tab() == "profiles" { "active" } else { "" }, "aria-current":if tab() == "profiles" { "page" } else { "false" }, onclick:move |_|tab.set("profiles".into()), "Profiles" }
            button { class:if tab() == "providers" { "active" } else { "" }, "aria-current":if tab() == "providers" { "page" } else { "false" }, onclick:move |_|tab.set("providers".into()), "Providers" }
            button { class:if tab() == "models" { "active" } else { "" }, "aria-current":if tab() == "models" { "page" } else { "false" }, onclick:move |_|tab.set("models".into()), "Model favorites" }
            button { class:if tab() == "appearance" { "active" } else { "" }, "aria-current":if tab() == "appearance" { "page" } else { "false" }, onclick:move |_|tab.set("appearance".into()), "Appearance" }
            button { class:if tab() == "keybinds" { "active" } else { "" }, "aria-current":if tab() == "keybinds" { "page" } else { "false" }, onclick:move |_|tab.set("keybinds".into()), "Keybinds" }
        }
        // A keyed list makes Dioxus replace the scrolling element when the tab changes.
        for active_tab in [tab()] {
        div { key:"{active_tab}", class:if active_tab == "models" { "settings-content models-settings" } else { "settings-content" },
        if tab() == "profiles" { ProfileSettings { key:"{profile_id}" } }
        if tab() == "providers" { ProviderSettings { key:"{profile_id}" } }
        if tab() == "models" {
            h3{"Model favorites"}p{class:"muted","Pins are stored on this server and shared with its other clients."}
            button{onclick:move |_|{spawn(network::refresh_metadata(ui));},"Refresh probes and model catalog"}
            crate::input::BrowserInput { placeholder:"Filter models", initial_value:filter(), onedit:move |snapshot: crate::input::InputSnapshot| { if *filter.peek() != snapshot.text { filter.set(snapshot.text); } }, aria_label:"Filter model favorites" }
            div{class:"models",for (index, model) in models.iter().cloned().enumerate(){
                if show_divider && index == favorite_count { div { class:"favorites-divider", role:"separator" } }
                div{class:"model-row",
                    div{strong{"{model.name}"}div{class:"muted","{model.model.provider.as_str()}/{model.model.api_name}"}}
                    button{"aria-label":if model.pinned{"Unpin model"}else{"Pin model"},onclick:move |_|{spawn(network::pin_model(ui,model.clone()));},if model.pinned{"★"}else{"☆"}}
                }
            }}
        }
        if tab() == "keybinds" { KeybindSettings {} }
        if tab() == "appearance" {
            h3{"Appearance"}
            label{r#for:"reasoning-display","Reasoning"}
            select{id:"reasoning-display",value:format!("{:?}",state.settings.preferences.reasoning_display),onchange:move |e|{
                ui.write().settings.preferences.reasoning_display=match e.value().as_str(){
                    "Hidden"=>client_shared::ReasoningDisplay::Hidden,
                    "Shown"=>client_shared::ReasoningDisplay::Shown,
                    _=>client_shared::ReasoningDisplay::Collapsed,
                };
            },option{value:"Hidden","Hidden"}option{value:"Collapsed","Collapsed"}option{value:"Shown","Shown"}}
            label{r#for:"turn-dividers","Dividers between model turns"}
            select{id:"turn-dividers",value:format!("{:?}",state.settings.preferences.turn_dividers),onchange:move |e|{
                ui.write().settings.preferences.turn_dividers=match e.value().as_str(){
                    "Hidden"=>client_shared::TurnDividerDisplay::Hidden,
                    "Line"=>client_shared::TurnDividerDisplay::Line,
                    _=>client_shared::TurnDividerDisplay::Labeled,
                };
            },option{value:"Labeled","Divider with labels"}option{value:"Line","Divider without labels"}option{value:"Hidden","Hidden"}}
            label{input{r#type:"checkbox",checked:state.settings.preferences.hide_text_recorded_results,onchange:move |e|ui.write().settings.preferences.hide_text_recorded_results=e.checked()},"Hide text recorded results"}
            label{input{r#type:"checkbox",checked:state.settings.preferences.hide_agent_statuses,onchange:move |e|ui.write().settings.preferences.hide_agent_statuses=e.checked()},"Hide agent statuses (interrupted always shown)"}
            label{input{r#type:"checkbox",checked:state.settings.preferences.double_click_session_opens_details,onchange:move |e|ui.write().settings.preferences.double_click_session_opens_details=e.checked()},"Open current session details when double-clicking a session"}
            label{r#for:"transcript-font-size","Transcript font size"}
            crate::interaction::RangeInput {
                id:"transcript-font-size", aria_label:"Transcript font size", min:11.0, max:22.0,
                value:f64::from(state.settings.preferences.transcript_font_size),
                preview_property:Some("--psa-transcript-font-size".to_string()), preview_unit:"px",
                oncommit:move |size: f64| {
                    let size = size.round().clamp(11.0, 22.0) as u16;
                    if ui.peek().settings.preferences.transcript_font_size != size { ui.write().settings.preferences.transcript_font_size=size; }
                },
            }
            label{r#for:"conversation-max-width","Conversation and composer max width (characters): {state.settings.preferences.conversation_max_width_ch}"}
            crate::interaction::RangeInput {
                id:"conversation-max-width", aria_label:"Conversation and composer max width", min:40.0, max:160.0,
                value:f64::from(state.settings.preferences.conversation_max_width_ch),
                preview_property:Some("--psa-conversation-width".to_string()), preview_unit:"rch",
                oncommit:move |width: f64| {
                    let width = width.round().clamp(40.0, 160.0) as u16;
                    if ui.peek().settings.preferences.conversation_max_width_ch != width { ui.write().settings.preferences.conversation_max_width_ch=width; }
                },
            }
        }
        }
        }
        }
    }}}
}

#[component]
fn KeybindSettings() -> Element {
    let sections: &[(&str, &[(&str, &str)])] = &[
        (
            "Workspace",
            &[
                ("Ctrl + W", "Close the active tab or empty pane"),
                ("Alt + 1–9", "Focus a tab by position in the active group"),
                ("Alt + Arrow keys", "Focus the neighboring workspace group"),
                (
                    "Arrow keys",
                    "Resize a focused split divider along its axis",
                ),
            ],
        ),
        (
            "Composer",
            &[
                (
                    "Enter",
                    "Send a message or run a command when the command menu is closed",
                ),
                ("Shift + Enter", "Insert a new line"),
                ("Ctrl + End", "Follow latest activity in the focused pane"),
                (
                    "Escape",
                    "Interrupt the agent when the command menu is closed (existing agent only)",
                ),
                ("Arrow Up / Down", "Navigate the open command menu"),
                ("Enter", "Select the highlighted command or option"),
                ("Tab", "Complete the highlighted slash command"),
                ("Escape", "Dismiss the command menu"),
            ],
        ),
        (
            "Dialogs and session creation",
            &[
                (
                    "Escape",
                    "Close Settings, agent information, or the directory picker",
                ),
                (
                    "Enter",
                    "Browse the entered absolute path in the directory picker",
                ),
                ("Enter", "Create a session from the session name field"),
                (
                    "Escape",
                    "Cancel session naming when creation is not pending",
                ),
            ],
        ),
    ];
    rsx! {
        div { class:"keybinds",
            h3 { "Keybinds" }
            p { class:"muted", "Application shortcuts are read-only. Workspace shortcuts are inactive while a dialog is open." }
            for (heading, shortcuts) in sections.iter() {
                section {
                    h4 { "{heading}" }
                    dl { class:"keybind-list",
                        for (keys, description) in shortcuts.iter() {
                            div { class:"keybind-row", dt { kbd { "{keys}" } } dd { "{description}" } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ProfileSettings() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let bootstrap = use_context::<crate::GuiBootstrap>();
    use_ui_subscription(|state| state.settings.clone());
    let state = ui.peek();
    let profile = state.settings.active_profile().clone();
    let profiles = state.settings.profiles.clone();
    let mut name = use_signal(|| profile.name.clone());
    let mut url = use_signal(|| profile.connection.api_url.clone());
    let mut key = use_signal(|| profile.connection.client_key.clone());
    let mut status = use_signal(|| None::<LocalServerStatus>);
    let mut pending = use_signal(|| false);
    let mut confirm = use_signal(|| None::<&'static str>);
    let mut delete_profile = use_signal(|| None::<String>);
    let platform = bootstrap.platform.clone();
    let generation = use_memo(move || ui.read().generation);
    use_effect(move || {
        let _ = generation();
        let platform = platform.clone();
        spawn(async move {
            match platform.local_server_status().await {
                Ok(value) => status.set(Some(value)),
                Err(_) => status.set(None),
            }
        });
    });
    let refresh_platform = bootstrap.platform.clone();
    let action_platform = bootstrap.platform.clone();
    let run_action = move |action: &'static str| {
        let platform = action_platform.clone();
        pending.set(true);
        confirm.set(None);
        spawn(async move {
            let result = match action {
                "start" => platform.local_server_start().await,
                "restart" => platform.local_server_restart().await,
                _ => platform.local_server_stop().await,
            };
            match result {
                Ok(value) => {
                    if let LocalServerStatus::Running { connection } = &value {
                        let current = ui.read().settings.active_profile_id == LOCAL_PROFILE_ID;
                        if current {
                            ui.write().change_connection(
                                connection.api_url.clone(),
                                connection.client_key.clone(),
                            );
                            ui.write().settings_open = true;
                        } else if let Some(local) = ui
                            .write()
                            .settings
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == LOCAL_PROFILE_ID)
                        {
                            local.connection = connection.clone();
                        }
                    }
                    status.set(Some(value));
                }
                Err(error) => ui.write().error = Some(error),
            }
            pending.set(false);
        });
    };
    rsx! {
        h3 { "Connection profiles" }
        div { class:"profile-list",
        for item in profiles {
            { let is_active = item.id == profile.id;
              let is_local = item.id == LOCAL_PROFILE_ID;
              let id = item.id.clone();
              let delete_id = item.id.clone();
              let confirm_id = item.id.clone();
              let refresh_platform = refresh_platform.clone();
              let mut run_start = run_action.clone();
              let mut run_confirm = run_action.clone();
              rsx! {
            article { class:if is_active { "profile-card active" } else { "profile-card" },
                div { class:"profile-card-heading",
                    div { strong { "{item.name}" } div { class:"muted", if is_local { "Local server" } else { "Remote server" } } }
                    button { class:if is_active { "profile-select selected" } else { "profile-select" }, disabled:is_active, "aria-label":if is_active { format!("{} selected", item.name) } else { format!("Select {}", item.name) }, onclick:move |_|ui.write().select_profile(id.clone()), if is_active { "Selected" } else { "Select" } }
                }
        if !is_local {
            button { class:"profile-delete", "aria-label":format!("Delete {}", item.name), onclick:move |_|delete_profile.set(Some(delete_id.clone())), "Delete profile" }
            if delete_profile.read().as_deref() == Some(item.id.as_str()) {
                div { class:"local-server-confirm", role:"alert", p { "Delete {item.name}?" }
                    button { onclick:move |_| {
                        if ui.read().settings.active_profile_id == confirm_id {
                            ui.write().select_profile(LOCAL_PROFILE_ID.into());
                        }
                        ui.write().settings.remove_profile(&confirm_id);
                        delete_profile.set(None);
                    }, "Confirm delete" }
                    button { onclick:move |_|delete_profile.set(None), "Cancel" }
                }
            }
        }
        if is_local {
            div { class:"local-server-status",
            p { role:"status", "Status: " match status.read().as_ref() { Some(LocalServerStatus::Stopped) => "Stopped", Some(LocalServerStatus::Running { .. }) => "Running", None => "Unavailable" } }
            button { disabled:pending(), onclick:move |_| {
                let platform = refresh_platform.clone();
                spawn(async move { match platform.local_server_status().await {
                    Ok(value) => status.set(Some(value)),
                    Err(error) => ui.write().error = Some(error),
                }});
            }, "Refresh status" }
            }
            div { class:"local-server-actions",
            button { disabled:pending() || matches!(status(), Some(LocalServerStatus::Running { .. })), onclick:move |_|run_start("start"), "Start" }
            button { disabled:pending() || !matches!(status(), Some(LocalServerStatus::Running { .. })), onclick:move |_|confirm.set(Some("restart")), "Restart" }
            button { disabled:pending() || !matches!(status(), Some(LocalServerStatus::Running { .. })), onclick:move |_|confirm.set(Some("stop")), "Stop" }
            }
            if let Some(action) = confirm() {
                div { class:"local-server-confirm", role:"alert", p { "{action} the local server? Active work may be interrupted." }
                    button { onclick:move |_|run_confirm(action), "Confirm" }
                    button { onclick:move |_|confirm.set(None), "Cancel" }
                }
            }
        } else if is_active {
            form { onsubmit:move |event: FormEvent| {
                event.prevent_default(); event.stop_propagation();
                let Some(profile_name) = form_text(&event, "profile-name") else { return; };
                let Some(api_url) = form_text(&event, "api-url") else { return; };
                let Some(client_key) = form_text(&event, "api-key") else { return; };
                if profile_name.trim().is_empty() || Client::new(&api_url,&client_key).is_err() { ui.write().error=Some("Enter a profile name and valid API URL and client key".into()); return; }
                let mut state = ui.write();
                state.settings.active_profile_mut().name = profile_name;
                state.change_connection(api_url,client_key);
                state.settings_open = true;
            },
            label { r#for:"profile-name", "Profile name" } crate::input::BrowserInput { id:"profile-name", name:"profile-name", aria_label:"Profile name", initial_value:name(), onedit:move |snapshot: crate::input::InputSnapshot| { if *name.peek() != snapshot.text { name.set(snapshot.text); } } }
            label{r#for:"api-url","API URL"}crate::input::BrowserInput { id:"api-url", name:"api-url", aria_label:"API URL", initial_value:url(), onedit:move |snapshot: crate::input::InputSnapshot| { if *url.peek() != snapshot.text { url.set(snapshot.text); } } }
            label{r#for:"api-key","API client key"}crate::input::BrowserInput { id:"api-key", name:"api-key", aria_label:"API client key", r#type:"password", initial_value:key(), onedit:move |snapshot: crate::input::InputSnapshot| { if *key.peek() != snapshot.text { key.set(snapshot.text); } } }
            p{class:"muted","Saved in desktop.toml as plaintext with owner-only permissions where supported. Provider credentials are never stored here."}
            button{class:"primary",r#type:"submit","Save connection and reconnect"}
            }
        }
            }
              }
            }
        }
        button { class:"profile-add", onclick:move |_| {
            let profile = ConnectionProfile::new_remote("New remote", ConnectionSettings::default());
            let id = profile.id.clone();
            ui.write().settings.profiles.push(profile);
            ui.write().select_profile(id);
        }, "＋ Add remote profile" }
        }
    }
}

fn form_text(event: &FormEvent, name: &str) -> Option<String> {
    match event.get_first(name)? {
        dioxus::html::FormValue::Text(text) => Some(text),
        dioxus::html::FormValue::File(_) => None,
    }
}

#[component]
fn ProviderSettings() -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut providers = use_signal(Vec::<ProviderInfo>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0u64);
    let generation = use_memo(move || ui.read().generation);
    use_effect(move || {
        let selected_generation = generation();
        let _ = refresh();
        loading.set(true);
        error.set(None);
        providers.set(Vec::new());
        spawn(async move {
            let result = match network::client(ui) {
                Ok(api) => api.list_providers().await,
                Err(error) => Err(error),
            };
            if ui.read().generation != selected_generation {
                return;
            }
            match result {
                Ok(items) => providers.set(items),
                Err(_) => error.set(Some(
                    "Unable to load providers. Check the server connection and retry.".into(),
                )),
            }
            loading.set(false);
        });
    });
    rsx! {
        h3 { "Providers" }
        if loading() { p { role:"status", "Loading providers…" } }
        if let Some(message) = error() {
            div { class:"error", role:"alert", "{message}" }
            button { onclick:move |_| refresh += 1, "Retry" }
        }
        if !loading() && error().is_none() && providers.read().is_empty() {
            p { class:"muted", "No providers available." }
        }
        div { class:"profile-list",
            for provider in providers() {
                ProviderCard { key:"{provider.id.as_str()}", id:provider.id, authed:provider.authed, providers }
            }
        }
    }
}

#[component]
pub(crate) fn ProviderCard(
    id: ProviderType,
    authed: bool,
    mut providers: Signal<Vec<ProviderInfo>>,
) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut editing = use_signal(|| false);
    let mut credential = use_signal(String::new);
    let mut confirming = use_signal(|| false);
    let mut pending = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let name = id.name();
    let credential_name = match id {
        ProviderType::Openrouter => "API key",
        ProviderType::OpenAiCodex => "Access token",
    };
    let mut save = move |value: Option<String>| {
        let generation = ui.read().generation;
        let api = match network::client(ui) {
            Ok(api) => api,
            Err(_) => {
                error.set(Some("Unable to connect to the server.".into()));
                return;
            }
        };
        pending.set(true);
        error.set(None);
        spawn(async move {
            let result = api
                .set_provider_credential(&SetProviderCredential {
                    provider: id,
                    credential: value,
                })
                .await;
            if ui.read().generation != generation {
                return;
            }
            match result {
                Ok(updated) => {
                    if let Some(item) = providers.write().iter_mut().find(|p| p.id == id) {
                        *item = updated;
                    }
                    credential.set(String::new());
                    editing.set(false);
                    confirming.set(false);
                    spawn(network::refresh_metadata(ui));
                }
                Err(_) => error.set(Some(
                    "Unable to update provider credentials. Please try again.".into(),
                )),
            }
            pending.set(false);
        });
    };
    let mut remove = save.clone();
    rsx! {
        article { class:"profile-card provider-card",
            div { class:"profile-card-heading",
                div { strong { "{name}" } div { class:"muted", if authed { "Configured" } else { "Not configured" } } }
                if !editing() {
                    button { disabled:pending(), "aria-label":format!("{} credentials for {}", if authed { "Edit" } else { "Add" }, name), onclick:move |_| { confirming.set(false); error.set(None); editing.set(true); }, if authed { "Edit" } else { "Add" } }
                }
            }
            if let Some(message) = error() { p { class:"error", role:"alert", "{message}" } }
            if editing() {
                form { onsubmit:move |event: FormEvent| {
                    event.prevent_default(); event.stop_propagation();
                    if pending() { return; }
                    if let Some(value) = form_text(&event, "credential") {
                        if !value.trim().is_empty() { save(Some(value)); }
                    }
                },
                label { r#for:format!("provider-credential-{}", id.as_str()), "{credential_name}" }
                crate::input::BrowserInput { id:format!("provider-credential-{}", id.as_str()), name:"credential", aria_label:credential_name, r#type:"password", required:true, initial_value:credential(), disabled:pending(), onedit:move |snapshot: crate::input::InputSnapshot| { if *credential.peek() != snapshot.text { credential.set(snapshot.text); } } }
                p { class:"muted", "Enter a new credential" }
                div { class:"provider-actions",
                    button { class:"primary", r#type:"submit", disabled:pending(), "Save" }
                    button { r#type:"button", disabled:pending(), onclick:move |_| { credential.set(String::new()); editing.set(false); error.set(None); }, "Cancel" }
                }
                }
            }
            if authed && !editing() && !confirming() {
                button { class:"profile-delete", disabled:pending(), "aria-label":format!("Delete credentials for {}", name), onclick:move |_| { error.set(None); confirming.set(true); }, "Delete credentials" }
            }
            if confirming() {
                div { class:"local-server-confirm", role:"alert", p { "Delete credentials for {name}?" }
                    button { disabled:pending(), onclick:move |_|remove(None), "Confirm delete" }
                    button { disabled:pending(), onclick:move |_|confirming.set(false), "Cancel" }
                }
            }
        }
    }
}
