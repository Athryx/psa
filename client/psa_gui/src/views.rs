use crate::{app::Ui, network};
use client_shared::*;
use dioxus::{core::spawn_forever, prelude::*};
use llm_api::{
    api::ContentBlock,
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::*;

#[cfg(test)]
#[path = "follow_tests.rs"]
mod follow_tests;
#[cfg(test)]
#[path = "tab_spinner_tests.rs"]
mod tab_spinner_tests;

#[component]
pub fn Rail() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    rsx! {nav{class:"rail","aria-label":"Workspace navigation",
        button{title:"Session browser","aria-label":"Session browser","aria-pressed":!ui.read().sidebar.collapsed && ui.read().sidebar.expanded_mode == SidebarMode::Browser,onclick:move |_|ui.write().sidebar.toggle_mode(SidebarMode::Browser),"▤"}
        button{title:"Current session and agents","aria-label":"Current session and agents","aria-pressed":!ui.read().sidebar.collapsed && ui.read().sidebar.expanded_mode == SidebarMode::Details,onclick:move |_|ui.write().sidebar.toggle_mode(SidebarMode::Details),"♧"}
        div{class:"spacer"}
        button{title:"Settings",onclick:move |_|ui.write().settings_open=true,"⚙"}
    }}
}

#[component]
pub fn Sidebar() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut frozen_order = use_signal(|| None::<Vec<SessionId>>);
    let state = ui.read();
    if state.sidebar.expanded_mode == SidebarMode::Details {
        return rsx! {aside{class:"sidebar",Details{}}};
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
    rsx! {aside{class:"sidebar",
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
                    summary{"{group.working_directory.display()}" if group.busy{span{class:"busy"," ●"}}}
                    if probe.is_none() { div { class: "folder-probe", ProbeName { id: group.probe_id } } }
                    for session in group.sessions{
                        button{key:"{session.id.0}",class:"session",title:"{session.name}",
                            onclick:move |_|{spawn(network::open_session(ui,session.id,false));},
                            ondoubleclick:move |_|{spawn(network::double_click_session(ui,session.id));},
                            if session.parent_id.is_some(){"⑂ "}
                            "{session.name}"
                            if session.busy{span{class:"busy"," ●"}}else if state.connection!="Connected"{span{class:"muted"," ?"}}
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
    let probe = ui
        .read()
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
                    header { h2 { "Probe information" } button { "aria-label": "Close probe information", onclick: move |_| { open.set(false); if let Some(node) = trigger() { spawn(async move { let _ = node.set_focus(true).await; }); } }, "×" } }
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
    let state = ui.read();
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
    rsx! {
        div{class:"sessions-heading", h2{"Current session"}}
        strong{class:"details-session-name","{session.name}"}
        dl{class:"details-grid",
            dt{"Working directory"}dd{"{session.working_directory.display()}"}
            dt{"Probe"}dd{ProbeName{id:session.probe_id}}
            dt{"Session default model"}dd{"{session.model_config.model.provider.as_str()}/{session.model_config.model.api_name}"}
            dt{"Default effort"}dd{"{session.model_config.effort.as_str()}"}
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
    let mut mounted = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut resizing = use_signal(|| false);
    let mut bounds = use_signal(|| None::<(f64, f64)>);
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
            let mouse_path = path.clone();
            rsx! {div{class:if vertical{"split vertical"}else{"split"},
                onmounted: move |event| mounted.set(Some(event.data())),
                onmousemove: move |event| {
                    if resizing() && let Some((origin, size)) = bounds() {
                        let point = event.client_coordinates();
                        if let Some(value) = drag_ratio(if vertical { point.y } else { point.x }, origin, size) {
                            set_ratio(&mut ui.write().workspace.root, &mouse_path, value);
                        }
                    }
                },
                onmouseup: move |_| { resizing.set(false); bounds.set(None); },
                onmouseleave: move |_| { resizing.set(false); bounds.set(None); },
                div{class:"branch",style:"flex:{ratio};",Layout{node:*first,path:first_path}}
                div{class:"divider",role:"separator",tabindex:0,title:"Drag to resize split; arrow keys also resize","aria-label":"Resize split",
                    "aria-orientation": if vertical { "horizontal" } else { "vertical" },
                    "aria-valuemin":"10","aria-valuemax":"90","aria-valuenow":(ratio*100.0).round().to_string(),
                    onmousedown: move |event| {
                        event.prevent_default(); event.stop_propagation();
                        resizing.set(true);
                        bounds.set(None);
                        let node = mounted.read().clone();
                        spawn(async move {
                            if let Some(node) = node && let Ok(rect) = node.get_client_rect().await && resizing() {
                                bounds.set(Some(if vertical { (rect.origin.y, rect.size.height) } else { (rect.origin.x, rect.size.width) }));
                            }
                        });
                    },
                    onkeydown: move |event| {
                        let delta = match event.key() {
                            Key::ArrowLeft if !vertical => -0.02,
                            Key::ArrowRight if !vertical => 0.02,
                            Key::ArrowUp if vertical => -0.02,
                            Key::ArrowDown if vertical => 0.02,
                            _ => return,
                        };
                        event.prevent_default();
                        set_ratio(&mut ui.write().workspace.root, &path, ratio + delta);
                    }
                }
                div{class:"branch",style:"flex:{1.0-ratio};",Layout{node:*second,path:second_path}}
            }}
        }
    }
}

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
    let state = ui.read();
    let focused = state.workspace.focused_group == group.id;
    let active = group
        .tabs
        .iter()
        .find(|t| Some(t.id) == group.active)
        .cloned();
    rsx! {section{class:if focused{"pane focused"}else{"pane"}, "data-pane-id":"{group.id.0}",
        onmousedown:move |_|{ui.write().workspace.focused_group=group.id;},
        onfocusin:move |_|{ui.write().workspace.focused_group=group.id;},
        ondragover:move |event|event.prevent_default(),
        ondrop:move |event|{event.prevent_default();let dragging=ui.read().dragging;if let Some(view)=dragging{ui.write().workspace.move_tab(view,group.id,usize::MAX);}ui.write().dragging=None;},
        div{class:"tabs",
            for (index,tab) in group.tabs.iter().cloned().enumerate(){
                div{key:"{tab.id.0}",class:format!("tab {} {}",if Some(tab.id)==group.active{"active"}else{""},if tab.preview{"preview"}else{""}),draggable:"true",
                    ondragstart:move |_|{ui.write().dragging=Some(tab.id);},
                    ondragend:move |_|ui.write().dragging=None,
                    ondragover:move |e|e.prevent_default(),
                    ondrop:move |e|{e.prevent_default();e.stop_propagation();let dragging=ui.read().dragging;if let Some(view)=dragging{ui.write().workspace.move_tab(view,group.id,index);}ui.write().dragging=None;},
                    button{class:"title",onclick:move |_|{ui.write().workspace.focus(tab.id);if tab.target.agent_id != crate::app::DRAFT_ROOT { ui.write().last_agents.insert(tab.target.session_id,tab.target.agent_id); } crate::app::request_composer_focus(ui);},ondoubleclick:move |_|ui.write().workspace.promote(tab.id),
                        "{tab_title(&state,tab.target)}"
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
        if state.dragging.is_some(){div{class:"drop-edges",
            for (edge,label,axis,before) in [
                ("left","Split left",SplitAxis::Horizontal,true),
                ("right","Split right",SplitAxis::Horizontal,false),
                ("top","Split above",SplitAxis::Vertical,true),
                ("bottom","Split below",SplitAxis::Vertical,false),
            ]{
                div{class:"drop-zone {edge}",ondragover:move |e|e.prevent_default(),ondrop:move |e|{e.prevent_default();e.stop_propagation();let dragging=ui.read().dragging;if let Some(view)=dragging{let mut state=ui.write();move_to_split(&mut state.workspace,view,group.id,axis,before);state.dragging=None;}},"{label}"}
            }
        }}
        for tab in group.tabs.iter().cloned(){div{key:"{tab.id.0}","data-view-id":"{tab.id.0}","data-active":if Some(tab.id)==group.active{"true"}else{"false"},style:if Some(tab.id)==group.active{"display:flex;flex-direction:column;flex:1;min-height:0;"}else{"display:none;"},TranscriptView{key:"{tab.id.0}:{tab.target.session_id.0}:{tab.target.agent_id.0}",active:Some(tab.id)==group.active,tab:tab.clone()}}}
        if active.is_none(){div{class:"empty","Select an agent to open a preview here."}}
    }}
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

fn update_following_from_scroll(mut following: Signal<bool>, near_bottom: bool) {
    // A redundant signal write reruns the follow effect, pulling a user who is
    // scrolling within the bottom threshold straight back to the end.
    if *following.peek() != near_bottom {
        following.set(near_bottom);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HistoryAction {
    Reveal,
    Fetch,
    None,
}

fn history_trigger(top: f64, active: bool, latched: &mut bool) -> bool {
    if top > 80.0 {
        *latched = false;
        return false;
    }
    if !active || *latched {
        return false;
    }
    *latched = true;
    true
}

fn history_action(count: usize, visible: usize, has_older: bool, busy: bool) -> HistoryAction {
    if count > visible {
        HistoryAction::Reveal
    } else if has_older && !busy {
        HistoryAction::Fetch
    } else {
        HistoryAction::None
    }
}

#[derive(Clone, PartialEq, Eq)]
struct HistorySnapshot {
    count: usize,
    boundary: Option<u64>,
    latest: Option<u64>,
    pending: std::collections::HashSet<String>,
}

fn updated_history_limit(
    limit: usize,
    previous: Option<(usize, Option<u64>)>,
    count: usize,
    boundary: Option<u64>,
    live_added: usize,
) -> usize {
    match previous {
        None => count.min(150),
        Some((previous_count, old_boundary)) if old_boundary == boundary => {
            (limit + count.saturating_sub(previous_count)).min(count)
        }
        _ => (limit + live_added).min(count),
    }
}

fn history_needs_continuation(old_height: f64, new_height: f64, client_height: f64) -> bool {
    new_height <= client_height + 1.0 || new_height <= old_height + 1.0
}

fn restored_history_top(old_height: f64, old_top: f64, new_height: f64) -> f64 {
    (old_top + new_height - old_height).max(0.0)
}

fn scroll_transcript_to_bottom(element: Signal<Option<std::rc::Rc<MountedData>>>) {
    if let Some(node) = element.peek().clone() {
        spawn(async move {
            // Scroll the transcript itself, not a sentinel via scrollIntoView: the
            // latter can move ancestor panes and may already be considered visible.
            let _ = node
                .scroll(
                    dioxus::html::geometry::PixelsVector2D::new(0.0, 1.0e9),
                    ScrollBehavior::Instant,
                )
                .await;
        });
    }
}

#[component]
fn TranscriptView(tab: Tab, active: bool) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut following = use_signal(|| ui.read().settings.preferences.auto_follow);
    let mut visible = use_signal(|| {
        ui.read()
            .transcripts
            .get(&tab.target)
            .map(|t| (t.committed.len() + t.pending.len()).min(150))
            .unwrap_or(150)
    });
    let mut history_snapshot = use_signal(|| {
        ui.read()
            .transcripts
            .get(&tab.target)
            .map(|t| HistorySnapshot {
                count: t.committed.len() + t.pending.len(),
                boundary: t.next_before,
                latest: t.committed.last_key_value().map(|(id, _)| *id),
                pending: t.pending.keys().cloned().collect(),
            })
    });
    // Each pane owns its trigger and scroll anchor, even for duplicate tabs.
    let mut top_latched = use_signal(|| false);
    let mut scroll_metrics = use_signal(|| (0.0_f64, 0.0_f64, 0.0_f64));
    let mut restore = use_signal(|| None::<(f64, f64, f64)>);
    let mut load_request = use_signal(|| 0_u64);
    let mut load_steps = use_signal(|| 0_usize);
    let mut history_busy = use_signal(|| false);
    let mut history_waiting = use_signal(|| false);
    let mut history_error = use_signal(|| false);
    let mut history_auto_fill = use_signal(|| false);
    let mut view_active = use_signal(|| active);
    let state = ui.read();
    let target = tab.target;
    let transcript = state.transcripts.get(&target);
    let rows = transcript
        .map(|t| t.recent_rows(visible()))
        .unwrap_or_default();
    let queries = transcript
        .map(|t| t.queries.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let call_ids: std::collections::HashSet<_> = rows
        .iter()
        .filter_map(|r| {
            if let psa_api::events::EventType::ToolCall(call) = &r.kind {
                Some(call.call_id.clone())
            } else {
                None
            }
        })
        .collect();
    let tool_results: std::collections::HashMap<_, _> = transcript
        .map(|t| {
            t.tool_exchanges_for(&call_ids)
                .into_iter()
                .filter_map(|exchange| exchange.result.map(|r| (exchange.call_id, r)))
                .collect()
        })
        .unwrap_or_default();
    let tool_results = std::rc::Rc::new(tool_results);
    let mut element = use_signal(|| None::<std::rc::Rc<MountedData>>);
    use_effect(use_reactive((&active,), move |(active,)| {
        view_active.set(active)
    }));
    use_effect(move || {
        let state = ui.read();
        let Some(t) = state.transcripts.get(&target) else {
            return;
        };
        let snapshot = HistorySnapshot {
            count: t.committed.len() + t.pending.len(),
            boundary: t.next_before,
            latest: t.committed.last_key_value().map(|(id, _)| *id),
            pending: t.pending.keys().cloned().collect(),
        };
        let old = history_snapshot.peek().clone();
        if old.as_ref() == Some(&snapshot) {
            return;
        }
        let live_added = old
            .as_ref()
            .map(|previous| {
                let start = match previous.latest {
                    Some(id) => std::ops::Bound::Excluded(id),
                    None => std::ops::Bound::Included(previous.boundary.unwrap_or(0)),
                };
                let committed = t
                    .committed
                    .range((start, std::ops::Bound::Unbounded))
                    .count();
                let added = snapshot.pending.difference(&previous.pending).count();
                let removed = previous.pending.difference(&snapshot.pending).count();
                (committed + added).saturating_sub(removed)
            })
            .unwrap_or(0);
        drop(state);
        let limit = *visible.peek();
        let old_summary = old.map(|old| (old.count, old.boundary));
        let new_limit = updated_history_limit(
            limit,
            old_summary,
            snapshot.count,
            snapshot.boundary,
            live_added,
        );
        if limit != new_limit {
            visible.set(new_limit);
        }
        history_snapshot.set(Some(snapshot));
    });
    let changed = use_memo(move || {
        let state = ui.read();
        state
            .transcripts
            .get(&target)
            .map(|t| {
                (
                    t.cursor,
                    t.pending.values().map(|p| p.revision).sum::<u64>(),
                )
            })
            .unwrap_or_default()
    });
    use_effect(move || {
        if let Some((old_height, old_top, client_height)) = restore() {
            restore.set(None);
            if let Some(node) = element.peek().clone() {
                spawn(async move {
                    if let Ok(size) = node.get_scroll_size().await {
                        // Do not override a user who moved while layout was measured.
                        if (scroll_metrics.peek().1 - old_top).abs() > 1.0 {
                            history_busy.set(false);
                            return;
                        }
                        let top = if *history_auto_fill.peek() && *following.peek() {
                            (size.height - client_height).max(0.0)
                        } else {
                            restored_history_top(old_height, old_top, size.height)
                        };
                        let _ = node
                            .scroll(
                                dioxus::html::geometry::PixelsVector2D::new(0.0, top),
                                ScrollBehavior::Instant,
                            )
                            .await;
                        // Invisible rows and short pages need another page without
                        // requiring an impossible additional scroll gesture. Bound
                        // automatic continuation for a misbehaving server.
                        if history_needs_continuation(old_height, size.height, client_height)
                            && *view_active.peek()
                        {
                            scroll_metrics.set((size.height, top, client_height));
                            load_steps += 1;
                            if *load_steps.peek() >= 10 {
                                // Yield between batches while retaining cancellation on unmount.
                                tokio::task::yield_now().await;
                                load_steps.set(0);
                            }
                            load_request += 1;
                            return;
                        }
                    }
                    history_busy.set(false);
                });
            } else {
                history_busy.set(false);
            }
        }
    });
    use_effect(move || {
        if load_request() == 0 {
            return;
        }
        let state = ui.peek();
        let transcript = state.transcripts.get(&target);
        let count = transcript
            .map(|t| t.committed.len() + t.pending.len())
            .unwrap_or(0);
        let has_older = transcript.is_some_and(|t| t.has_older);
        let generation = state.generation;
        let busy = state.history_requests.get(&target) == Some(&generation);
        drop(state);
        let (height, top, client_height) = *scroll_metrics.peek();
        let limit = *visible.peek();
        match history_action(count, limit, has_older, busy) {
            HistoryAction::Reveal => {
                if !*history_auto_fill.peek() {
                    following.set(false);
                }
                restore.set(Some((height, top, client_height)));
                visible.set((limit + 150).min(count));
            }
            HistoryAction::Fetch => {
                // Do not allow a short tail limit to expose shared rows before
                // this pane can capture and restore its anchor.
                visible.set(limit.min(count));
                if !*history_auto_fill.peek() {
                    following.set(false);
                }
                ui.write().history_requests.insert(target, generation);
                dioxus::core::spawn_forever(async move {
                    let loaded = network::older(ui, target).await;
                    let owns_request = ui.read().history_requests.get(&target) == Some(&generation);
                    if owns_request {
                        ui.write().history_requests.remove(&target);
                    }
                    if loaded && ui.read().generation == generation {
                        if let Ok(metrics) = scroll_metrics.try_peek() {
                            let (height, top, client_height) = *metrics;
                            if top <= 80.0 {
                                if let (Ok(mut anchor), Ok(mut limit)) =
                                    (restore.try_write(), visible.try_write())
                                {
                                    *anchor = Some((height, top, client_height));
                                    let count = ui
                                        .peek()
                                        .transcripts
                                        .get(&target)
                                        .map(|t| t.committed.len() + t.pending.len())
                                        .unwrap_or(0);
                                    *limit = (*limit + 150).min(count);
                                    return;
                                }
                            }
                        }
                    } else {
                        if let Ok(mut latch) = top_latched.try_write() {
                            *latch = false;
                        }
                        if let Ok(mut error) = history_error.try_write() {
                            *error = true;
                        }
                    }
                    if let Ok(mut busy) = history_busy.try_write() {
                        *busy = false;
                    }
                });
            }
            HistoryAction::None => {
                if busy && has_older {
                    history_waiting.set(true);
                } else {
                    history_busy.set(false);
                }
            }
        }
    });
    // Other panes may already be fetching this target. Wait for that request,
    // then reveal from this pane's own anchor rather than issuing a duplicate.
    use_effect(move || {
        if !history_waiting() {
            return;
        }
        let state = ui.read();
        let busy = state.history_requests.get(&target) == Some(&state.generation);
        drop(state);
        if !busy {
            history_waiting.set(false);
            if *view_active.peek() && scroll_metrics.peek().1 <= 80.0 {
                load_request += 1;
            } else {
                history_busy.set(false);
            }
        }
    });
    // An initially short (or newly activated) pane cannot emit a scroll event.
    use_effect(use_reactive((&active,), move |(active,)| {
        let node = element.read().clone();
        let state = ui.read();
        let has_history = state
            .transcripts
            .get(&target)
            .is_some_and(|t| t.has_older || t.committed.len() + t.pending.len() > *visible.peek());
        drop(state);
        if !active
            || !has_history
            || *history_busy.peek()
            || *history_error.peek()
            || *top_latched.peek()
        {
            return;
        }
        if let Some(node) = node {
            history_busy.set(true);
            spawn(async move {
                let (size, offset, rect) = (
                    node.get_scroll_size().await,
                    node.get_scroll_offset().await,
                    node.get_client_rect().await,
                );
                if let (Ok(size), Ok(offset), Ok(rect)) = (size, offset, rect) {
                    if *view_active.peek() && size.height <= rect.size.height + 1.0 {
                        scroll_metrics.set((size.height, offset.y, rect.size.height));
                        top_latched.set(true);
                        history_auto_fill.set(true);
                        load_steps.set(0);
                        load_request += 1;
                        return;
                    }
                }
                history_busy.set(false);
            });
        }
    }));
    let follow_request = use_memo(move || ui.read().follow_request);
    let mut seen_follow_request = use_signal(|| follow_request.peek().0);
    use_effect(move || {
        let (revision, view) = follow_request();
        if revision != *seen_follow_request.peek() {
            seen_follow_request.set(revision);
            if view == Some(tab.id) {
                following.set(true);
                scroll_transcript_to_bottom(element);
            }
        }
    });
    use_effect(use_reactive((&active,), move |(active,)| {
        let _ = changed();
        // Subscribe to mounting as well as transcript updates. Hidden tabs are
        // scrolled when activated, after their layout becomes visible again.
        let _ = element.read();
        if active && following() {
            scroll_transcript_to_bottom(element);
        }
    }));
    let overflow_anchor = if history_busy() { "none" } else { "auto" };
    rsx! {
        div{class:"transcript",style:"font-size:{state.settings.preferences.transcript_font_size}px;--conversation-max-width:{state.settings.preferences.conversation_max_width_ch}rch;overflow-anchor:{overflow_anchor}",
            onmounted:move |event|element.set(Some(event.data())),
            onscroll:move |event|{
                let top = event.scroll_top();
                let height = event.scroll_height() as f64;
                scroll_metrics.set((height, top, event.client_height() as f64));
                let near_bottom = (event.scroll_height()-event.client_height()) as f64-top<80.0;
                if !near_bottom {
                    history_auto_fill.set(false);
                    update_following_from_scroll(following, false);
                } else if !*history_busy.peek() && (top > 80.0 || !*top_latched.peek()) {
                    update_following_from_scroll(following, true);
                }
                let mut latched = *top_latched.peek();
                let short_follow = *following.peek() && height <= event.client_height() as f64 + 1.0;
                let trigger = !short_follow && history_trigger(top, active, &mut latched);
                if latched != *top_latched.peek() {
                    top_latched.set(latched);
                }
                if trigger && !*history_busy.peek() {
                    history_auto_fill.set(false);
                    history_error.set(false);
                    history_busy.set(true);
                    load_steps.set(0);
                    load_request += 1;
                }
            },
            onwheel:move |event| {
                let upward = event.delta().strip_units().y < 0.0;
                if active && upward && *history_busy.peek() {
                    history_auto_fill.set(false);
                    following.set(false);
                }
                if active && upward && !*history_busy.peek() {
                    if let Some(node) = element.peek().clone() {
                        top_latched.set(true);
                        history_auto_fill.set(false);
                        history_error.set(false);
                        history_busy.set(true);
                        spawn(async move {
                            let (size, offset, rect) = (node.get_scroll_size().await, node.get_scroll_offset().await, node.get_client_rect().await);
                            if let (Ok(size), Ok(offset), Ok(rect)) = (size, offset, rect) {
                                if offset.y <= 80.0 {
                                    scroll_metrics.set((size.height, offset.y, rect.size.height));
                                    load_steps.set(0);
                                    load_request += 1;
                                    return;
                                }
                            }
                            top_latched.set(false);
                            history_busy.set(false);
                        });
                    }
                }
            },
            if history_error() { button { onclick: move |_| { history_auto_fill.set(false); history_error.set(false); history_busy.set(true); load_steps.set(0); load_request += 1; }, "Retry loading history" } }
            for row in rows.into_iter().filter(|row|!matches!(&row.kind,psa_api::events::EventType::ToolResult(result) if call_ids.contains(&result.call_id))){
                EventRow{key:"{row.id:?}",kind:row.kind,provisional:row.provisional,results:tool_results.clone()}
            }
            for query in queries{Question{key:"{query.id}",query}}
            if !following(){button{class:"new-activity",onclick:move |_|{following.set(true);scroll_transcript_to_bottom(element);},"Follow new activity ↓"}}
        }
        if target.agent_id == crate::app::DRAFT_ROOT { DraftComposer { tab } } else { Composer{tab} }
    }
}

fn blocks(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Refusal(refusal) => refusal.text.clone(),
            ContentBlock::Image(_) => "[Image attachment]".into(),
            ContentBlock::Audio(audio) => audio
                .transcript
                .clone()
                .unwrap_or_else(|| "[Audio attachment]".into()),
            ContentBlock::File(file) => format!(
                "[File attachment: {}]",
                file.filename.as_deref().unwrap_or("unnamed")
            ),
            ContentBlock::ProviderContent(content) => format!(
                "[Provider content: {} / {}]",
                content.provider.as_str(),
                content.content_type
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[derive(Props, Clone)]
pub(crate) struct EventRowProps {
    kind: psa_api::events::EventType,
    provisional: bool,
    results: std::rc::Rc<std::collections::HashMap<String, psa_api::events::ToolResult>>,
}
impl PartialEq for EventRowProps {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}
#[allow(non_snake_case)]
pub(crate) fn EventRow(props: EventRowProps) -> Element {
    let EventRowProps {
        kind,
        provisional,
        results,
    } = props;
    use psa_api::events::EventType;
    let preferences = try_use_context::<Signal<Ui>>()
        .map(|ui| ui.read().settings.preferences.clone())
        .unwrap_or_default();
    match kind {
        EventType::Message(message) => {
            if matches!(message.message_type, psa_api::events::MessageType::System) {
                let content = blocks(&message.content);
                return rsx! {details{class:"tool system-prompt",summary{"System Prompt" if let Some(description) = &message.description { " · {description}" }}pre{"{content}"}}};
            }
            let is_agent = matches!(
                message.message_type,
                psa_api::events::MessageType::Agent | psa_api::events::MessageType::Subagent
            );
            let is_human = matches!(message.message_type, psa_api::events::MessageType::Human);
            rsx! {article{class:if is_human { "message human-message" } else if is_agent { "message agent-message" } else { "message" },
                if !is_human && !is_agent { div{class:"role","{message.message_type:?}"} }
                if provisional { span { class:"streaming-indicator", "Streaming" } }
                if is_agent {
                    for block in &message.content {
                        match block {
                            ContentBlock::Text(text) => rsx! { crate::markdown::Markdown { text: text.text.clone() } },
                            _ => rsx! { pre { "{blocks(std::slice::from_ref(block))}" } },
                        }
                    }
                } else { pre { "{blocks(&message.content)}" } }
            }}
        }
        EventType::Log(log) => {
            rsx! {article{class:if log.error{"message error"}else{"message muted"},div{class:"role","Harness log"}pre{"{log.message}"}}}
        }
        EventType::ToolCall(call) => {
            let result = results.get(&call.call_id);
            let status = result
                .map(|r| if r.error { "error" } else { "complete" })
                .unwrap_or("running / awaiting result");
            let preview = call.arguments.chars().take(100).collect::<String>();
            rsx! {details{class:if result.is_some_and(|r|r.error){"tool error"}else{"tool"},summary{"{call.tool_name} · {status} · {preview}"}h4{"Arguments"}pre{"{call.arguments}"}if let Some(result)=result{h4{"Result"}pre{"{blocks(&result.content)}"}}}}
        }
        EventType::ToolResult(result) => {
            rsx! {details{class:"tool",summary{"Unmatched tool result · {result.call_id}"}pre{"{blocks(&result.content)}"}}}
        }
        EventType::Compaction(compaction) => {
            rsx! {details{class:"tool",summary{"Context compaction"}pre{"{serde_json::to_string_pretty(&compaction).unwrap_or_default()}"}}}
        }
        EventType::ProviderItem(item) => {
            rsx! {details{class:"tool",summary{"Provider-native event"}pre{"{serde_json::to_string_pretty(&item).unwrap_or_default()}"}}}
        }
        EventType::Thinking(thinking) => {
            let text = thinking.contents.summary.join("\n");
            if text.is_empty() {
                return rsx! {};
            }
            match preferences.reasoning_display {
                ReasoningDisplay::Hidden => rsx! {},
                ReasoningDisplay::Collapsed => {
                    rsx! {details{class:"tool reasoning",summary{"Reasoning"}pre{"{text}"}}}
                }
                ReasoningDisplay::Shown => rsx! {article{class:"message reasoning",pre{"{text}"}}},
            }
        }
        EventType::Result(result) => {
            if preferences.hide_text_recorded_results && matches!(result, AgentResult::Text(_)) {
                rsx! {}
            } else {
                rsx! {details{class:"tool",summary{"Recorded result"}pre{"{serde_json::to_string_pretty(&result).unwrap_or_default()}"}}}
            }
        }
        EventType::InferenceResultEnd(end) => match end.outcome {
            llm_api::api::Outcome::Failed(error) => {
                rsx! {article{class:"message error",role:"alert",
                    div{class:"role","Inference failed"}
                    pre{"{error}"}
                    details{summary{"Error details"}pre{"{serde_json::to_string_pretty(&error).unwrap_or_default()}"}}
                }}
            }
            llm_api::api::Outcome::Cancelled => {
                rsx! {article{class:"message muted",div{class:"role","Inference cancelled"}}}
            }
            llm_api::api::Outcome::Incomplete { reason } => {
                use llm_api::api::StopReason;
                let reason = match reason {
                    StopReason::EndTurn => "End of turn".to_string(),
                    StopReason::ToolCalls => "Tool calls".to_string(),
                    StopReason::StopSequence => "Stop sequence".to_string(),
                    StopReason::TokenLimit => "Token limit reached".to_string(),
                    StopReason::ContentFilter => "Content filtered".to_string(),
                    StopReason::Other(reason) => reason,
                };
                rsx! {article{class:"message",div{class:"role","Inference incomplete"}pre{"{reason}"}}}
            }
            llm_api::api::Outcome::Completed { .. } | llm_api::api::Outcome::InProgress => rsx! {},
        },
        EventType::StatusChange(status) => {
            if status.new_status == AgentState::Interrupted {
                rsx! {div{class:"error","Agent Interrupted"}}
            } else if preferences.hide_agent_statuses {
                rsx! {}
            } else {
                rsx! {div{class:"muted","Agent {status.new_status:?}"}}
            }
        }
        _ => rsx! {},
    }
}

#[cfg(test)]
#[path = "appearance_event_tests.rs"]
mod appearance_event_tests;
#[cfg(test)]
#[path = "inference_result_tests.rs"]
mod inference_result_tests;
#[cfg(test)]
#[path = "transcript_visual_tests.rs"]
mod transcript_visual_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftCommandStage {
    Completion,
    Configurations,
    Models,
    SessionModels,
    SessionEfforts,
}

fn draft_command_stage(
    text: &str,
    dismissed: bool,
    choosing_model: bool,
) -> Option<DraftCommandStage> {
    if dismissed {
        return None;
    }
    if choosing_model {
        return Some(DraftCommandStage::Models);
    }
    if text
        .strip_prefix("/agent")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        return Some(DraftCommandStage::Configurations);
    }
    if text
        .strip_prefix("/model")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        return Some(DraftCommandStage::SessionModels);
    }
    if text
        .strip_prefix("/effort")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        return Some(DraftCommandStage::SessionEfforts);
    }
    (!complete_commands(text).is_empty()).then_some(DraftCommandStage::Completion)
}

fn draft_model_label(model: &ModelSelection) -> String {
    match model {
        ModelSelection::Global | ModelSelection::Parent => {
            "Session default model and effort".into()
        }
        ModelSelection::Fixed { model, effort } => {
            let effort = match effort {
                EffortSelection::Global | EffortSelection::Parent => "session effort".to_string(),
                EffortSelection::Fixed(effort) => effort.as_str().to_string(),
            };
            format!("{model} · {effort}")
        }
    }
}

fn select_draft_config(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    target: TranscriptTarget,
    id: ConfigId,
) -> bool {
    let mut state = ui.write();
    let choosing_model = if let Some(draft) = state.draft_roots.get_mut(&session_id) {
        draft.select_config(id);
        draft.model_choices().len() > 1
    } else {
        false
    };
    state.drafts.remove(&target);
    choosing_model
}

fn refresh_draft_configs(mut ui: Signal<Ui>, session_id: SessionId, target: TranscriptTarget) {
    {
        let mut state = ui.write();
        let Some(draft) = state.draft_roots.get_mut(&session_id) else {
            return;
        };
        if draft.loading || draft.pending {
            return;
        }
        draft.ready = false;
        state.drafts.remove(&target);
    }
    spawn(network::load_draft_root(ui, session_id));
}

#[cfg(test)]
#[path = "draft_composer_tests.rs"]
mod draft_composer_tests;

#[cfg(test)]
#[path = "status_tests.rs"]
mod status_tests;

#[component]
fn ComposerStatus(
    path: Option<String>,
    model: Option<String>,
    effort: Option<Effort>,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
) -> Element {
    let usage = format_context_usage(context_tokens, context_window);
    let effort_class = effort
        .map(|e| format!("effort-{}", e.as_str()))
        .unwrap_or_default();
    rsx! {
        div { class:"composer-status", "aria-label":"Composer status",
            span { class:"composer-status-path", title:"{path.as_deref().unwrap_or(\"Unknown working directory\")}", "{path.as_deref().unwrap_or(\"Unknown working directory\")}" }
            span { class:"composer-status-details",
                span { class:"composer-status-model", "{model.as_deref().unwrap_or(\"Unknown model\")}" }
                if let Some(effort) = effort { " · " span { class:"{effort_class}", "{effort.as_str()}" } }
                span { class:"composer-status-usage", title:"Last inference token usage; not exact current context", "{usage}" }
            }
        }
    }
}

fn format_context_usage(tokens: Option<u64>, window: Option<u64>) -> String {
    let percent = match (tokens, window) {
        (Some(tokens), Some(window)) if window > 0 => {
            format!(
                "{}%",
                (u128::from(tokens) * 100 + u128::from(window) / 2) / u128::from(window)
            )
        }
        _ => "?".to_string(),
    };
    let window = window
        .filter(|window| *window > 0)
        .map(format_token_count)
        .unwrap_or_else(|| "?".to_string());
    format!("({percent} / {window})")
}

fn draft_status_model(
    selection: Option<&ModelSelection>,
    session: Option<&ModelConfig>,
) -> (Option<Model>, Option<Effort>) {
    match selection {
        Some(ModelSelection::Global | ModelSelection::Parent) => session
            .map(|config| (Some(config.model.clone()), Some(config.effort)))
            .unwrap_or_default(),
        Some(ModelSelection::Fixed { model, effort }) => {
            let effort = match effort {
                EffortSelection::Fixed(effort) => Some(*effort),
                EffortSelection::Global | EffortSelection::Parent => {
                    session.map(|config| config.effort)
                }
            };
            (Model::parse_slug(model).ok(), effort)
        }
        None => (None, None),
    }
}

fn format_token_count(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    format!("{}k", (u128::from(count) + 500) / 1_000)
}

fn info_location(state: &Ui, session_id: SessionId) -> (Option<String>, Option<ProbeId>) {
    let Some(session) = state.sessions.iter().find(|s| s.id == session_id) else {
        return (None, None);
    };
    let path = Some(session.working_directory.to_string_lossy().into_owned());
    let probe = Some(session.probe_id);
    (path, probe)
}

fn token_difference(total: Option<u64>, part: Option<u64>) -> Option<u64> {
    total?.checked_sub(part?)
}

#[component]
fn ResolvedTemplate(id: AgentDefinitionId) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut name = use_signal(|| None::<String>);
    use_effect(move || {
        name.set(None);
        spawn(async move {
            if let Ok(client) = network::client(ui) {
                if let Ok(Some(snapshot)) = client.get_resolved_agent_config(id).await {
                    name.set(Some(snapshot.config.config_id.name));
                }
            }
        });
    });
    rsx! { "{name.read().as_deref().unwrap_or(\"Unknown\")}" }
}

#[component]
fn AgentInfo(
    name: String,
    template: Option<String>,
    definition_id: Option<AgentDefinitionId>,
    path: Option<String>,
    probe: Option<ProbeId>,
    model: Option<String>,
    effort: Option<Effort>,
    usage: Option<AgentUsage>,
    mut open: Signal<bool>,
) -> Element {
    crate::app::register_modal(open);
    let count = |value: Option<u64>| {
        value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "Unknown".into())
    };
    let input = usage.as_ref().and_then(|u| u.input_tokens);
    let cached = usage.as_ref().and_then(|u| u.cached_input_tokens);
    let cache_write = usage.as_ref().and_then(|u| u.cache_write_tokens);
    let hit = usage
        .as_ref()
        .and_then(cache_hit_rate)
        .map(|rate| format!("{:.1}%", rate * 100.0))
        .unwrap_or_else(|| "Unknown".into());
    let output = usage.as_ref().and_then(|u| u.output_tokens);
    let thinking = usage.as_ref().and_then(|u| u.reasoning_output_tokens);
    let effort_class = effort
        .map(|e| format!("effort-{}", e.as_str()))
        .unwrap_or_default();
    rsx! {
        div { class:"modal-backdrop", onclick:move |event| { event.stop_propagation(); open.set(false); },
            section { class:"settings agent-info", role:"dialog", "aria-modal":"true", "aria-label":"Agent information", tabindex:"-1",
                onclick:move |event| event.stop_propagation(),
                onmounted:move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
                onkeydown:move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); open.set(false); } },
                header { h2 { "Agent information" } button { "aria-label":"Close agent information", onclick:move |_| open.set(false), "×" } }
                dl { class:"info-grid",
                    dt { "Name" } dd { "{name}" }
                    dt { "Template" } dd { if let Some(id) = definition_id { ResolvedTemplate { id } } else { "{template.as_deref().unwrap_or(\"Unknown\")}" } }
                    dt { "Path" } dd { "{path.as_deref().unwrap_or(\"Unknown\")}" }
                    dt { "Probe" } dd { if let Some(id) = probe { ProbeName { id } } else { "Unknown" } }
                    dt { "Model" } dd { "{model.as_deref().unwrap_or(\"Unknown\")}" " · " span { class:"{effort_class}", "{effort.map(|e| e.as_str()).unwrap_or(\"Unknown\")}" } }
                    dt { "Input tokens" } dd { "{count(input)}" }
                    dt { class:"info-detail", "Cached" } dd { "{count(cached)}" }
                    dt { class:"info-detail", "Cache hit rate" } dd { "{hit}" }
                    dt { class:"info-detail", "Uncached" } dd { "{count(token_difference(input, cached))}" }
                    if let Some(cache_write) = cache_write { dt { class:"info-detail", "Cache write" } dd { "{cache_write}" } }
                    dt { "Output tokens" } dd { "{count(output)}" }
                    dt { class:"info-detail", "Thinking" } dd { "{count(thinking)}" }
                    dt { class:"info-detail", "Non-thinking" } dd { "{count(token_difference(output, thinking))}" }
                    dt { "Total tokens" } dd { "{count(usage.as_ref().and_then(|u| u.total_tokens))}" }
                }
            }
        }
    }
}

type MenuNodes = Signal<Vec<Option<std::rc::Rc<MountedData>>>>;

fn register_menu_option(mut nodes: MenuNodes, index: usize, node: std::rc::Rc<MountedData>) {
    let mut nodes = nodes.write();
    if nodes.len() <= index {
        nodes.resize_with(index + 1, || None);
    }
    nodes[index] = Some(node);
}

fn scroll_menu_option(
    popup: Signal<Option<std::rc::Rc<MountedData>>>,
    nodes: MenuNodes,
    selected: Signal<usize>,
    index: usize,
) {
    let popup = popup.peek().clone();
    let option = nodes.peek().get(index).and_then(Clone::clone);
    if let (Some(popup), Some(option)) = (popup, option) {
        spawn(async move {
            let (Ok(container), Ok(item), Ok(offset)) = (
                popup.get_client_rect().await,
                option.get_client_rect().await,
                popup.get_scroll_offset().await,
            ) else {
                return;
            };
            let delta = menu_scroll_delta(
                container.origin.y as f64,
                container.size.height as f64,
                item.origin.y as f64,
                item.size.height as f64,
            );
            if delta != 0.0 && *selected.peek() == index {
                let _ = popup
                    .scroll(
                        dioxus::html::geometry::PixelsVector2D::new(offset.x, offset.y + delta),
                        ScrollBehavior::Instant,
                    )
                    .await;
            }
        });
    }
}

fn menu_scroll_delta(top: f64, height: f64, item_top: f64, item_height: f64) -> f64 {
    if item_top < top {
        item_top - top
    } else {
        (item_top + item_height - top - height).max(0.0)
    }
}

#[component]
fn DraftComposer(tab: Tab) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let target = tab.target;
    let session_id = target.session_id;
    let mut selected_index = use_signal(|| 0usize);
    let mut menu_popup = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let menu_nodes = use_signal(Vec::new);
    let mut dismissed = use_signal(|| false);
    let mut choosing_model = use_signal(|| false);
    let mut all_models = use_signal(|| false);
    let mut textarea_node = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let mut info_open = use_signal(|| false);
    // Submission promotes this draft to a real agent, unmounting this component.
    // Its task must outlive the composer until the request outcome is handled.
    use_effect(move || {
        spawn(network::load_draft_root(ui, session_id));
    });
    let state = ui.read();
    let text = state.drafts.get(&target).cloned().unwrap_or_default();
    let draft = state.draft_roots.get(&session_id);
    let stage = draft_command_stage(&text, dismissed(), choosing_model());
    let filter = text
        .strip_prefix("/agent")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let configs: Vec<_> = draft
        .map(|d| {
            d.configs
                .iter()
                .filter(|c| c.config.config_id.name.to_lowercase().contains(&filter))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let key_configs = configs.clone();
    let commands = complete_commands(&text);
    let key_commands = commands.clone();
    let current = state
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.model_config.clone());
    let (status_model, status_effort) =
        draft_status_model(draft.and_then(|d| d.model.as_ref()), current.as_ref());
    let status_info = status_model
        .as_ref()
        .and_then(|model| state.models.iter().find(|info| info.model == *model));
    let status_name = status_info.map(|info| info.name.clone()).or_else(|| {
        status_model
            .as_ref()
            .map(|model| model.model_name().to_owned())
    });
    let status_window = status_info.and_then(|info| info.context_window);
    let info_name = draft
        .and_then(|d| d.selected.as_ref())
        .map(|id| id.name.clone())
        .unwrap_or_else(|| "Unselected root agent".into());
    let (info_path, info_probe) = info_location(&state, session_id);
    let default_effort = current.as_ref().map(|c| c.effort).unwrap_or(Effort::Medium);
    let model_filter = text
        .strip_prefix("/model")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let session_models: Vec<_> = state
        .models
        .iter()
        .filter(|m| {
            (all_models() || m.pinned || current.as_ref().is_some_and(|c| c.model == m.model))
                && (model_filter.is_empty()
                    || m.name.to_lowercase().contains(&model_filter)
                    || m.model.api_name.to_lowercase().contains(&model_filter))
        })
        .cloned()
        .collect();
    let key_session_models = session_models.clone();
    let effort_filter = text
        .strip_prefix("/effort")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let efforts = session_efforts(&state.models, current.as_ref(), &effort_filter);
    let key_efforts = efforts.clone();
    let models = draft.map(|d| d.model_choices()).unwrap_or_default();
    let key_models = models.clone();
    let pending = draft.is_none_or(|d| d.loading || d.pending);
    let blocked = pending
        || draft.is_some_and(|d| d.uncertain)
        || matches!(parse_composer_input(&text), ComposerInput::Command { .. });
    let count = match stage {
        Some(DraftCommandStage::Completion) => commands.len(),
        Some(DraftCommandStage::Configurations) => configs.len(),
        Some(DraftCommandStage::Models) => models.len(),
        Some(DraftCommandStage::SessionModels) => session_models.len(),
        Some(DraftCommandStage::SessionEfforts) => efforts.len(),
        None => 0,
    };
    rsx! { div { class:"composer", style:"--conversation-max-width:{state.settings.preferences.conversation_max_width_ch}rch",
        if info_open() { AgentInfo { name:info_name.clone(), template:draft.and_then(|d| d.selected.as_ref()).map(|id| id.name.clone()), definition_id:None, path:info_path, probe:info_probe, model:status_name.clone(), effort:status_effort, usage:None, open:info_open } }
        if let Some(stage) = stage { div { class:"popup command-popup", role:"listbox", onmounted:move |event| menu_popup.set(Some(event.data())),
            "aria-label": match stage { DraftCommandStage::Completion => "Slash commands", DraftCommandStage::Configurations => "Agent configurations", DraftCommandStage::Models => "Root agent model policy", DraftCommandStage::SessionModels => "Session default model", DraftCommandStage::SessionEfforts => "Session default effort" },
            match stage {
                DraftCommandStage::Completion => rsx! {
                    for (index, command) in commands.iter().copied().enumerate() {
                        button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index, onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),
                            onmousedown:move |event| event.prevent_default(),
                            onclick:move |_| { if command.id == CommandId::Refresh { refresh_draft_configs(ui, session_id, target); } else if command.id == CommandId::Info { ui.write().drafts.remove(&target); info_open.set(true); } else { ui.write().drafts.insert(target, format!("/{} ", command.name)); } selected_index.set(0); if command.id != CommandId::Info { if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } } },
                            span { class:"command-title", "/{command.name}" }
                            span { class:"command-description", "{command.description}" }
                        }
                    }
                    p { class:"command-hint", "Tab to complete · Enter to select · Esc to dismiss" }
                },
                DraftCommandStage::Configurations => rsx! {
                    strong { class:"command-heading", "Choose a root agent" }
                    if configs.is_empty() { p { class:"command-empty", "No matching usable configurations." } }
                    for (index, config) in configs.iter().enumerate() {
                        { let id = config.config.config_id.clone(); let name = id.name.clone(); let group = format!("{:?}", id.config_group); let description = config.config.description.clone(); rsx! {
                            button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index, onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),
                                onmousedown:move |event| event.prevent_default(),
                                onclick:move |_| { choosing_model.set(select_draft_config(ui, session_id, target, id.clone())); selected_index.set(0); dismissed.set(false); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                                span { class:"command-title", "{name}" }
                                span { class:"command-description", "{group}" if !description.is_empty() { " · {description}" } }
                            }
                        }}
                    }
                    p { class:"command-hint", "↑↓ to navigate · Enter to select · Esc to dismiss" }
                },
                DraftCommandStage::SessionModels => rsx! {
                    strong { class:"command-heading", "Session default model (not individual agents)" }
                    if let Some(config)=&current { p { class:"muted", "Current: {config.model.provider.as_str()}/{config.model.api_name} · {config.effort.as_str()}" } }
                    button { onmousedown:move |event| event.prevent_default(), onclick:move |_| { all_models.set(!all_models()); selected_index.set(0); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } }, if all_models() { "Show favorites" } else { "All models…" } }
                    if session_models.is_empty() { p { class:"command-empty", if model_filter.is_empty() { "No favorite models. Open settings to pin models, or choose All models." } else { "No matching models. Try another search or choose All models." } } }
                    for (index, model) in session_models.iter().cloned().enumerate() {
                        button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index, onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),
                            onmousedown:move |event| event.prevent_default(),
                            onclick:move |_| { spawn(change_model(ui, session_id, ModelConfig { model:model.model.clone(), effort:choose_effort(&model, default_effort) })); ui.write().drafts.remove(&target); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                            span { class:"command-title", "{model.name}" }
                            span { class:"command-description", "{model.model.provider.as_str()}/{model.model.api_name}" }
                        }
                    }
                    p { class:"command-hint", "↑↓ to navigate · Enter to select · Esc to dismiss" }
                },
                DraftCommandStage::SessionEfforts => rsx! {
                    strong { class:"command-heading", "Session default effort (not individual agents)" }
                    if let Some(config)=&current { p { class:"muted", "Current: {config.effort.as_str()}" } }
                    if efforts.is_empty() { p { class:"command-empty", "No matching effort options for the current model." } }
                    for (index, effort) in efforts.iter().copied().enumerate() {
                        button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index, onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),
                            onmousedown:move |event| event.prevent_default(),
                            onclick:move |_| { select_session_effort(ui, target, effort); selected_index.set(0); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                            span { class:"command-title", "{effort.as_str()}" }
                        }
                    }
                    p { class:"command-hint", "↑↓ to navigate · Enter to select · Esc to dismiss" }
                },
                DraftCommandStage::Models => rsx! {
                    strong { class:"command-heading", "Choose the root agent model policy" }
                    if models.is_empty() { p { class:"command-empty", "No model policies available." } }
                    for (index, model) in models.iter().cloned().enumerate() {
                        { let label = draft_model_label(&model); rsx! {
                            button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index, onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),
                                onmousedown:move |event| event.prevent_default(),
                                onclick:move |_| { if let Some(draft)=ui.write().draft_roots.get_mut(&session_id) { draft.model=Some(model.clone()); } choosing_model.set(false); selected_index.set(0); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                                span { class:"command-title", "{label}" }
                            }
                        }}
                    }
                    p { class:"command-hint", "↑↓ to navigate · Enter to select · Esc to dismiss" }
                }
            }
        } }
        div { class:"composer-input",
          div { class:"composer-grow",
            div { class:"composer-mirror", "aria-hidden":"true", "{text} " }
        textarea { value:text.clone(), rows:"1", "aria-label":"Message to agent", disabled:pending,
            onmounted:move |event| textarea_node.set(Some(event.data())),
            oninput:move |event| { let mut state=ui.write(); state.workspace.promote(tab.id); state.drafts.insert(target,event.value()); selected_index.set(0); dismissed.set(false); choosing_model.set(false); },
            onkeydown:move |event| {
                if let Some(stage) = stage {
                    match event.key() {
                        Key::ArrowDown => { event.prevent_default(); if count > 0 { let next = (selected_index()+1)%count; selected_index.set(next); scroll_menu_option(menu_popup, menu_nodes, selected_index, next); } }
                        Key::ArrowUp => { event.prevent_default(); if count > 0 { let next = (selected_index()+count-1)%count; selected_index.set(next); scroll_menu_option(menu_popup, menu_nodes, selected_index, next); } }
                        Key::Escape => { event.prevent_default(); event.stop_propagation(); dismissed.set(true); choosing_model.set(false); }
                        Key::Tab if stage == DraftCommandStage::Completion && !event.modifiers().contains(Modifiers::SHIFT) => {
                            event.prevent_default(); if let Some(command)=key_commands.get(selected_index()) { ui.write().drafts.insert(target,if command.id == CommandId::Refresh { "/refresh".into() } else { format!("/{} ", command.name) }); selected_index.set(0); }
                        }
                        Key::Enter if !event.modifiers().contains(Modifiers::SHIFT) => {
                            event.prevent_default();
                            match stage {
                                DraftCommandStage::Completion => if let Some(command)=key_commands.get(selected_index()) { if command.id == CommandId::Refresh { refresh_draft_configs(ui,session_id,target); } else if command.id == CommandId::Info { ui.write().drafts.remove(&target); info_open.set(true); } else { ui.write().drafts.insert(target,format!("/{} ", command.name)); } selected_index.set(0); }
                                DraftCommandStage::Configurations => if let Some(config)=key_configs.get(selected_index()) {
                                    choosing_model.set(select_draft_config(ui,session_id,target,config.config.config_id.clone())); selected_index.set(0);
                                },
                                DraftCommandStage::SessionModels => if let Some(model)=key_session_models.get(selected_index()) {
                                    spawn(change_model(ui,session_id,ModelConfig { model:model.model.clone(), effort:choose_effort(model,default_effort) })); ui.write().drafts.remove(&target); selected_index.set(0);
                                },
                                DraftCommandStage::SessionEfforts => if let Some(effort)=key_efforts.get(selected_index()) {
                                    select_session_effort(ui,target,*effort); selected_index.set(0);
                                },
                                DraftCommandStage::Models => if let Some(model)=key_models.get(selected_index()) {
                                    if let Some(draft)=ui.write().draft_roots.get_mut(&session_id) { draft.model=Some(model.clone()); }
                                    choosing_model.set(false); selected_index.set(0);
                                },
                            }
                        }
                        _ => {}
                    }
                } else if event.key()==Key::Enter && !event.modifiers().contains(Modifiers::SHIFT) {
                    event.prevent_default();
                    if matches!(parse_composer_input(&text), ComposerInput::Command { id: CommandId::Refresh, .. }) { refresh_draft_configs(ui,session_id,target); }
                    else if matches!(parse_composer_input(&text), ComposerInput::Command { id: CommandId::Info, .. }) { ui.write().drafts.remove(&target); info_open.set(true); }
                    else { spawn_forever(network::submit_draft_root(ui,session_id)); }
                }
            }
        }
          }
          button { class:"primary composer-send", "aria-label":"Send first message", disabled:blocked, onclick:move |_| { spawn_forever(network::submit_draft_root(ui,session_id)); }, "↑" }
        }
        ComposerStatus { path: state.sessions.iter().find(|s| s.id == session_id).map(|s| s.working_directory.to_string_lossy().into_owned()), model: status_name, effort: status_effort, context_tokens: None, context_window: status_window }
    }}
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod composer_tests;

#[component]
fn Composer(tab: Tab) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut all = use_signal(|| false);
    let mut selected = use_signal(|| 0usize);
    let mut menu_popup = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let menu_nodes = use_signal(Vec::new);
    let mut dismissed = use_signal(|| false);
    let mut info_open = use_signal(|| false);
    let state = ui.read();
    let target = tab.target;
    let status = state
        .transcripts
        .get(&target)
        .map(|transcript| agent_status(transcript, &state.models));
    let status_model = status
        .as_ref()
        .and_then(|status| status.model_config.as_ref())
        .map(|config| {
            state
                .models
                .iter()
                .find(|model| model.model == config.model)
                .map(|model| model.name.clone())
                .unwrap_or_else(|| config.model.model_name().to_owned())
        });
    let status_effort = status
        .as_ref()
        .and_then(|status| status.model_config.as_ref())
        .map(|config| config.effort);
    let status_tokens = status.as_ref().and_then(|status| status.context_tokens);
    let status_window = status.as_ref().and_then(|status| status.context_window);
    let draft = state.drafts.get(&target).cloned().unwrap_or_default();
    let uncertain = state.uncertain_submissions.contains(&target);
    let parsed = parse_composer_input(&draft);
    let info_command = matches!(
        parsed,
        ComposerInput::Command {
            id: CommandId::Info,
            ..
        }
    );
    let agent_command = matches!(
        parsed,
        ComposerInput::Command {
            id: CommandId::Agent,
            ..
        }
    );
    let refresh_command = matches!(
        parsed,
        ComposerInput::Command {
            id: CommandId::Refresh,
            ..
        }
    );
    let commands: Vec<_> = complete_commands(&draft)
        .into_iter()
        .filter(|c| {
            matches!(
                c.id,
                CommandId::SessionModel | CommandId::SessionEffort | CommandId::Info
            )
        })
        .collect();
    // A complete command name remains a completion until a space starts its arguments.
    let model_command = commands.is_empty()
        && matches!(
            parsed,
            ComposerInput::Command {
                id: CommandId::SessionModel,
                ..
            }
        );
    let effort_command = commands.is_empty()
        && matches!(
            parsed,
            ComposerInput::Command {
                id: CommandId::SessionEffort,
                ..
            }
        );
    let command = !dismissed() && (model_command || effort_command || !commands.is_empty());
    let current = state
        .sessions
        .iter()
        .find(|s| s.id == target.session_id)
        .map(|s| s.model_config.clone());
    let filter = draft
        .strip_prefix("/model")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let models: Vec<_> = state
        .models
        .iter()
        .filter(|m| {
            (all() || m.pinned || current.as_ref().is_some_and(|c| c.model == m.model))
                && (filter.is_empty()
                    || m.name.to_lowercase().contains(&filter)
                    || m.model.api_name.to_lowercase().contains(&filter))
        })
        .cloned()
        .collect();
    let key_models = models.clone();
    let default_effort = current.as_ref().map(|c| c.effort).unwrap_or(Effort::Medium);
    let effort_filter = draft
        .strip_prefix("/effort")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let efforts = session_efforts(&state.models, current.as_ref(), &effort_filter);
    let key_efforts = efforts.clone();
    let key_commands = commands.clone();
    let agent = state
        .agents
        .get(&target.session_id)
        .and_then(|agents| agents.iter().find(|agent| agent.id == target.agent_id));
    let info_name = agent
        .map(|agent| agent.name.clone())
        .unwrap_or_else(|| "Unknown agent".into());
    let info_usage = agent.map(|agent| agent.usage.clone());
    let (info_path, info_probe) = info_location(&state, target.session_id);
    rsx! {div{class:"composer",style:"--conversation-max-width:{state.settings.preferences.conversation_max_width_ch}rch",
        if info_open() { AgentInfo { name:info_name, template:None, definition_id:agent.map(|a| a.definition_id), path:info_path, probe:info_probe, model:status_model.clone(), effort:status_effort, usage:info_usage, open:info_open } }
        if command{div{class:"popup command-popup",role:"listbox","aria-label":"Slash commands",onmounted:move |event| menu_popup.set(Some(event.data())),
            if !model_command && !effort_command{
                div{class:"command-heading","Commands"}
                for (index, spec) in commands.iter().copied().enumerate() {
                    button{class:if selected()==index{"command-option selected"}else{"command-option"},role:"option","aria-selected":(selected()==index).to_string(),onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),onclick:move |_|{if spec.id == CommandId::Info { ui.write().drafts.remove(&target); info_open.set(true); } else { ui.write().drafts.insert(target,format!("/{} ", spec.name)); } selected.set(0);},
                        span{class:"command-title","/{spec.name}"} span{class:"command-description","{spec.description}"}
                    }
                }
                div{class:"command-hint","↑↓ navigate · Enter select · Tab complete · Esc dismiss"}
            }
            else if effort_command {
                div{class:"command-heading","Session default effort (not individual agents)"}
                if let Some(config)=&current { p{class:"muted","Current: {config.effort.as_str()}"} }
                if efforts.is_empty() { p{class:"command-empty","No matching effort options for the current model."} }
                for (index, effort) in efforts.iter().copied().enumerate() {
                    button{class:if selected()==index{"command-option selected"}else{"command-option"},role:"option","aria-selected":(selected()==index).to_string(),onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),onclick:move |_|{select_session_effort(ui,target,effort);selected.set(0);},
                        span{class:"command-title","{effort.as_str()}"}
                    }
                }
                div{class:"command-hint","↑↓ navigate · Enter select · Esc dismiss"}
            }
            else{
                div{class:"command-heading","Session default model (not individual agents)"}
                if let Some(config)=&current{p{class:"muted","Current: {config.model.provider.as_str()}/{config.model.api_name} · {config.effort.as_str()}"}}
                button{onclick:move |_|{all.set(!all());selected.set(0);},if all(){"Show favorites"}else{"All models…"}}
                if models.is_empty(){p{class:"command-empty",if filter.is_empty(){"No favorite models. Open settings to pin models, or choose All models."}else{"No matching models. Try another search or choose All models."}}}
                for (index,model) in models.iter().cloned().enumerate(){
                    button{class:if selected()==index{"command-option selected"}else{"command-option"},role:"option","aria-selected":(selected()==index).to_string(),onmounted:move |event| register_menu_option(menu_nodes, index, event.data()),onclick:move |_|{spawn(change_model(ui,target.session_id,ModelConfig{model:model.model.clone(),effort:choose_effort(&model,default_effort)}));ui.write().drafts.remove(&target);},
                        span{class:"command-title","{model.name}"} span{class:"command-description","{model.model.provider.as_str()}/{model.model.api_name}"}
                    }
                }
                div{class:"command-hint","↑↓ navigate · Enter select · Esc dismiss"}
            }
        }}
        if agent_command { p { class:"command-hint", "The /agent command is only available before creating the root agent. Use //agent to send it as text." } }
        if refresh_command { p { class:"command-hint", "The /refresh command is only available before creating the root agent. Use //refresh to send it as text." } }
        div { class:"composer-input",
          div { class:"composer-grow",
            div { class:"composer-mirror", "aria-hidden":"true", "{draft} " }
        textarea{value:draft.clone(),rows:"1", "aria-label":"Message to agent",
            oninput:move |event|{let mut s=ui.write();s.workspace.promote(tab.id);s.drafts.insert(target,event.value());selected.set(0);dismissed.set(false);},
            onkeydown:move |event|{
                if command{
                    match event.key(){
                        Key::ArrowDown=>{event.prevent_default();let count=if model_command{key_models.len()}else if effort_command{key_efforts.len()}else{key_commands.len()};if count > 0 { let next=(selected()+1)%count; selected.set(next); scroll_menu_option(menu_popup,menu_nodes,selected,next); }},
                        Key::ArrowUp=>{event.prevent_default();let count=if model_command{key_models.len()}else if effort_command{key_efforts.len()}else{key_commands.len()};if count > 0 { let next=(selected()+count-1)%count; selected.set(next); scroll_menu_option(menu_popup,menu_nodes,selected,next); }},
                        Key::Escape=>{event.prevent_default();event.stop_propagation();dismissed.set(true);},
                        Key::Tab if !model_command && !effort_command && !event.modifiers().contains(Modifiers::SHIFT)=>{event.prevent_default();if let Some(spec)=key_commands.get(selected()){ui.write().drafts.insert(target,format!("/{} ",spec.name));selected.set(0);}},
                        Key::Enter if !event.modifiers().contains(Modifiers::SHIFT)=>{event.prevent_default();if model_command{if let Some(model)=key_models.get(selected()){spawn(change_model(ui,target.session_id,ModelConfig{model:model.model.clone(),effort:choose_effort(model,default_effort)}));ui.write().drafts.remove(&target);}}else if effort_command {if let Some(effort)=key_efforts.get(selected()){select_session_effort(ui,target,*effort);selected.set(0);}}else if let Some(spec)=key_commands.get(selected()){if spec.id == CommandId::Info { ui.write().drafts.remove(&target); info_open.set(true); } else { ui.write().drafts.insert(target,format!("/{} ",spec.name)); } selected.set(0);}},
                        _=>{},
                    }
                } else if event.key()==Key::Escape { event.prevent_default(); event.stop_propagation(); let generation=ui.read().generation; spawn(async move { if ui.read().generation == generation { network::submit(ui,target,true).await; } }); }
                else if event.key()==Key::Enter&&!event.modifiers().contains(Modifiers::SHIFT){event.prevent_default();if info_command { ui.write().drafts.remove(&target); info_open.set(true); } else { ui.write().workspace.promote(tab.id);spawn(network::submit(ui,target,false)); }}
            }
        }
          }
          button{class:"primary composer-send","aria-label":"Send steer",disabled:uncertain || matches!(parsed, ComposerInput::Command { .. }),onclick:move |_|{ui.write().workspace.promote(tab.id);spawn(network::submit(ui,target,false));},"↑"}
        }
        ComposerStatus { path: state.sessions.iter().find(|s| s.id == target.session_id).map(|s| s.working_directory.to_string_lossy().into_owned()), model: status_model, effort: status_effort, context_tokens: status_tokens, context_window: status_window }
        if uncertain || state.transcript_status.get(&target).is_some_and(|status| status != "Live") {
            div{class:"composer-actions",
                if uncertain { button { onclick:move |_| { ui.write().uncertain_submissions.remove(&target); }, "I checked delivery; enable retry" } }
                if let Some(status) = state.transcript_status.get(&target).filter(|status| *status != "Live") { span{class:"muted","{status}"} }
            }
        }
    }}
}
fn session_efforts(
    models: &[ModelInfo],
    current: Option<&ModelConfig>,
    filter: &str,
) -> Vec<Effort> {
    current
        .and_then(|config| models.iter().find(|model| model.model == config.model))
        .map(|model| {
            model
                .reasoning_options
                .iter()
                .map(|option| option.effort)
                .filter(|effort| effort.as_str().contains(filter))
                .collect()
        })
        .unwrap_or_default()
}

fn effort_model_config(
    models: &[ModelInfo],
    current: Option<&ModelConfig>,
    effort: Effort,
) -> Option<ModelConfig> {
    if !session_efforts(models, current, "").contains(&effort) {
        return None;
    }
    current.cloned().map(|mut config| {
        config.effort = effort;
        config
    })
}

fn select_session_effort(mut ui: Signal<Ui>, target: TranscriptTarget, effort: Effort) {
    let config = {
        let state = ui.read();
        let current = state
            .sessions
            .iter()
            .find(|session| session.id == target.session_id)
            .map(|session| &session.model_config);
        effort_model_config(&state.models, current, effort)
    };
    if let Some(config) = config {
        let generation = ui.read().generation;
        spawn(async move {
            if ui.read().generation == generation {
                change_model(ui, target.session_id, config).await;
            }
        });
        ui.write().drafts.remove(&target);
    }
}

fn choose_effort(model: &ModelInfo, preferred: Effort) -> Effort {
    if model
        .reasoning_options
        .iter()
        .any(|o| o.effort == preferred)
    {
        preferred
    } else {
        model
            .reasoning_options
            .first()
            .map(|o| o.effort)
            .unwrap_or(Effort::Off)
    }
}
async fn change_model(mut ui: Signal<Ui>, session_id: SessionId, model_config: ModelConfig) {
    let generation = ui.read().generation;
    let Ok(api) = network::client(ui) else {
        return;
    };
    let result = api
        .update_session(
            session_id,
            &UpdateSession {
                model_config: Some(model_config),
                ..Default::default()
            },
        )
        .await;
    if generation != ui.read().generation {
        return;
    }
    match result {
        Ok(_) => {
            ui.write().notice = Some("Session default updated. Pinned agents are unchanged.".into())
        }
        Err(_) => {
            ui.write().error = Some("Unable to change the session default model/effort".into())
        }
    }
}

#[component]
fn Question(query: ClientQueryRequest) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let ClientQuery::AskQuestion(request) = &query.payload;
    let count = request.questions.len();
    let mut answers = use_signal(move || vec![String::new(); count]);
    let mut submitting = use_signal(|| false);
    let submit_query = query.clone();
    rsx! {section{class:"question",h4{"Agent needs your input"}
        for (index,question) in request.questions.iter().enumerate(){
            label{"{question.question}"}
            for choice in question.choices.clone(){button{onclick:move |_|answers.write()[index]=choice.clone(),"{choice}"}}
            input{value:answers.read()[index].clone(),"aria-label":question.question.clone(),oninput:move |event|answers.write()[index]=event.value()}
        }
        button{class:"primary",disabled:submitting()||answers.read().iter().any(|a|a.trim().is_empty()),onclick:move |_|{
            let query=submit_query.clone();let response=ClientQueryResponse::AskQuestion(AskQuestionResponse{answers:answers.read().iter().map(|answer|ClientAnswer{answer:answer.clone(),notes:None}).collect()});
            let generation = ui.read().generation;
            submitting.set(true);spawn(async move{
                let failed = match network::client(ui) {
                    Ok(api) => api.respond_to_client_query(query.session_id,query.agent_id,query.id,&response).await.is_err(),
                    Err(_) => true,
                };
                if ui.read().generation != generation { return; }
                if failed { ui.write().error=Some("Unable to answer question; it may have been answered elsewhere.".into()); }
                submitting.set(false);
            });
        },"Submit answers"}
    }}
}

#[component]
pub fn Settings() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut tab = use_signal(|| "profiles".to_string());
    let state = ui.read();
    let profile_id = state.settings.active_profile_id.clone();
    let mut filter = use_signal(String::new);
    let mut models: Vec<_> = state
        .models
        .iter()
        .filter(|m| {
            m.name.to_lowercase().contains(&filter().to_lowercase())
                || m.model
                    .api_name
                    .to_lowercase()
                    .contains(&filter().to_lowercase())
        })
        .cloned()
        .collect();
    models.sort_by_key(|model| !model.pinned);
    let favorite_count = models.iter().take_while(|model| model.pinned).count();
    let show_divider = favorite_count > 0 && favorite_count < models.len();
    rsx! {div{class:"modal-backdrop",onclick:move |event|{event.stop_propagation();ui.write().settings_open=false;},section{class:"settings settings-dialog",role:"dialog","aria-modal":"true","aria-label":"Settings",tabindex:"-1",onclick:move |event|event.stop_propagation(),
        onmounted:move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
        onkeydown:move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); ui.write().settings_open=false; } },
        header{h2{"Settings"}button{onclick:move |_|ui.write().settings_open=false,"Close ×"}}
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
            input{placeholder:"Filter models",value:filter(),oninput:move |event|filter.set(event.value()),"aria-label":"Filter model favorites"}
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
            label{input{r#type:"checkbox",checked:state.settings.preferences.hide_text_recorded_results,onchange:move |e|ui.write().settings.preferences.hide_text_recorded_results=e.checked()},"Hide text recorded results"}
            label{input{r#type:"checkbox",checked:state.settings.preferences.hide_agent_statuses,onchange:move |e|ui.write().settings.preferences.hide_agent_statuses=e.checked()},"Hide agent statuses (interrupted always shown)"}
            label{input{r#type:"checkbox",checked:state.settings.preferences.double_click_session_opens_details,onchange:move |e|ui.write().settings.preferences.double_click_session_opens_details=e.checked()},"Open current session details when double-clicking a session"}
            label{r#for:"transcript-font-size","Transcript font size"}input{id:"transcript-font-size",r#type:"range",min:"11",max:"22",value:state.settings.preferences.transcript_font_size.to_string(),oninput:move |e|{if let Ok(size)=e.value().parse(){ui.write().settings.preferences.transcript_font_size=size;}}}
            label{r#for:"conversation-max-width","Conversation and composer max width (characters): {state.settings.preferences.conversation_max_width_ch}"}
            input{id:"conversation-max-width",r#type:"range",min:"40",max:"160",value:state.settings.preferences.conversation_max_width_ch.to_string(),oninput:move |e|{if let Ok(width)=e.value().parse::<u16>(){ui.write().settings.preferences.conversation_max_width_ch=width.clamp(40,160);}}}
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
    let state = ui.read();
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
            label { r#for:"profile-name", "Profile name" } input { id:"profile-name", value:name(), oninput:move |e|name.set(e.value()) }
            label{r#for:"api-url","API URL"}input{id:"api-url",value:url(),oninput:move |event|url.set(event.value())}
            label{r#for:"api-key","API client key"}input{id:"api-key",r#type:"password",autocomplete:"off",value:key(),oninput:move |event|key.set(event.value())}
            p{class:"muted","Saved in desktop.toml as plaintext with owner-only permissions where supported. Provider credentials are never stored here."}
            button{class:"primary",onclick:move |_|{
                if name().trim().is_empty() || Client::new(&url(),&key()).is_err() { ui.write().error=Some("Enter a profile name and valid API URL and client key".into()); return; }
                ui.write().settings.active_profile_mut().name = name();
                ui.write().change_connection(url(),key());
                ui.write().settings_open = true;
            },"Save connection and reconnect"}
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
                label { r#for:format!("provider-credential-{}", id.as_str()), "{credential_name}" }
                input { id:format!("provider-credential-{}", id.as_str()), r#type:"password", autocomplete:"off", value:credential(), disabled:pending(), oninput:move |event|credential.set(event.value()) }
                p { class:"muted", "Enter a new credential" }
                div { class:"provider-actions",
                    button { class:"primary", disabled:pending() || credential().trim().is_empty(), onclick:move |_|save(Some(credential())), "Save" }
                    button { disabled:pending(), onclick:move |_| { credential.set(String::new()); editing.set(false); error.set(None); }, "Cancel" }
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
