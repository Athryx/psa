use crate::{app::Ui, network};
use client_shared::*;
use dioxus::prelude::*;
use llm_api::{
    api::ContentBlock,
    providers::{Effort, ModelConfig},
};
use psa_api::*;

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
        if groups.is_empty(){p{class:"muted","No sessions loaded. Check the connection or choose another probe."}}
        for group in groups{
            div{key:"{group.probe_id}:{group.working_directory.display()}",class:"folder",
                details{key: if creation_target.as_ref().is_some_and(|target| target.probe == group.probe_id && group.working_directory.as_os_str() == std::ffi::OsStr::new(&target.directory)) { state.creation_revision.to_string() } else { "0".to_string() },open:true,
                    summary{"{group.working_directory.display()}" if probe.is_none(){" · {group.probe_id}"} if group.busy{span{class:"busy"," ●"}}}
                    for session in group.sessions{
                        button{key:"{session.id.0}",class:"session",title:"{session.name}",
                            onclick:move |_|{spawn(network::open_session(ui,session.id,false));},
                            ondoubleclick:move |_|{spawn(network::open_session(ui,session.id,true));},
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
            div { class: "folder transient-folder", div { class: "transient-folder-title", "{target.directory} · {target.probe}" } crate::creation::InlineSessionName { target } }
        }
        if state.next_sessions.is_some(){button{onclick:move |_|{spawn(network::more_sessions(ui));},"Load more sessions"}p{class:"muted","Folder groups may be incomplete until all pages are loaded."}}
    }}
}

fn probe_label(probe: &Probe) -> String {
    let label = probe
        .metadata
        .label
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&probe.metadata.hostname);
    format!("{label} ({})", probe.metadata.id)
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
        h2{"Current session"}strong{"{session.name}"}
        dl{class:"details-grid",
            dt{"Working directory"}dd{"{session.working_directory.display()}"}
            dt{"Probe"}dd{"{session.probe_id}"}
            dt{"Session default model"}dd{"{session.model_config.model.provider.as_str()}/{session.model_config.model.api_name}"}
            dt{"Default effort"}dd{"{session.model_config.effort.as_str()}"}
            dt{"Activity"}dd{if state.connection!="Connected"||!state.fresh_sessions.contains(&session.id){"Unknown (awaiting refresh)"}else if session.busy{"Busy"}else{"Idle"}}
        }
        h2{"Agents"}
        button{onclick:move |_|{let show=ui.read().settings.preferences.show_hidden_agents;ui.write().settings.preferences.show_hidden_agents= !show;},if state.settings.preferences.show_hidden_agents{"Hide hidden agents"}else{"Reveal hidden agents"}}
        for row in tree{
            button{class:"agent",style:"padding-left:{12+row.depth*14}px",disabled:row.context_only,
                onclick:move |_|ui.write().open(TranscriptTarget{session_id:target.session_id,agent_id:row.agent.id},false),
                "{row.agent.name} · {row.agent.state:?}" if row.context_only{" (hidden ancestor)"}
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
    rsx! {section{class:if focused{"pane focused"}else{"pane"},
        onmousedown:move |_|{ui.write().workspace.focused_group=group.id;},
        ondragover:move |event|event.prevent_default(),
        ondrop:move |event|{event.prevent_default();let dragging=ui.read().dragging;if let Some(view)=dragging{ui.write().workspace.move_tab(view,group.id,usize::MAX);}ui.write().dragging=None;},
        div{class:"tabs",
            for (index,tab) in group.tabs.iter().cloned().enumerate(){
                div{key:"{tab.id.0}",class:format!("tab {} {}",if Some(tab.id)==group.active{"active"}else{""},if tab.preview{"preview"}else{""}),draggable:"true",
                    ondragstart:move |_|{ui.write().dragging=Some(tab.id);},
                    ondragend:move |_|ui.write().dragging=None,
                    ondragover:move |e|e.prevent_default(),
                    ondrop:move |e|{e.prevent_default();e.stop_propagation();let dragging=ui.read().dragging;if let Some(view)=dragging{ui.write().workspace.move_tab(view,group.id,index);}ui.write().dragging=None;},
                    button{class:"title",onclick:move |_|{ui.write().workspace.focus(tab.id);if tab.target.agent_id != crate::app::DRAFT_ROOT { ui.write().last_agents.insert(tab.target.session_id,tab.target.agent_id); }},ondoubleclick:move |_|ui.write().workspace.promote(tab.id),"{tab_title(&state,tab.target)}"}
                    button{class:"close",title:"Close tab",onclick:move |_|{ui.write().workspace.close(tab.id);},"×"}
                }
            }
            div{class:"tab-actions",
                button{title:"Split horizontally",onclick:move |_|{ui.write().workspace.split(group.id,SplitAxis::Horizontal);},"◫"}
                button{title:"Split vertically",onclick:move |_|{ui.write().workspace.split(group.id,SplitAxis::Vertical);},"⬒"}
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
        for tab in group.tabs.iter().cloned(){div{key:"{tab.id.0}",style:if Some(tab.id)==group.active{"display:flex;flex-direction:column;flex:1;min-height:0;"}else{"display:none;"},TranscriptView{tab:tab.clone()}}}
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
fn TranscriptView(tab: Tab) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut following = use_signal(|| ui.read().settings.preferences.auto_follow);
    let mut visible = use_signal(|| 150usize);
    let state = ui.read();
    let target = tab.target;
    let transcript = state.transcripts.get(&target);
    let count = transcript
        .map(|t| t.committed.len() + t.pending.len())
        .unwrap_or(0);
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
        let _ = changed();
        if following()
            && let Some(node) = element.read().clone()
        {
            spawn(async move {
                let _ = node.scroll_to(ScrollBehavior::Instant).await;
            });
        }
    });
    rsx! {
        div{class:"transcript",style:"font-size:{state.settings.preferences.transcript_font_size}px",
            onscroll:move |event|{let near=(event.scroll_height()-event.client_height()) as f64-event.scroll_top()<80.0;following.set(near);},
            if target.agent_id != crate::app::DRAFT_ROOT { button{onclick:move |_|{following.set(false);visible+=150;spawn(network::older(ui,target));},"Load older history"} }
            if count>visible(){button{onclick:move |_|visible+=150,"Show {count-visible()} earlier loaded items"}}
            for row in rows.into_iter().filter(|row|!matches!(&row.kind,psa_api::events::EventType::ToolResult(result) if call_ids.contains(&result.call_id))){
                EventRow{key:"{row.id:?}",kind:row.kind,provisional:row.provisional,results:tool_results.clone()}
            }
            for query in queries{Question{key:"{query.id}",query}}
            div{onmounted:move |event|element.set(Some(event.data())),style:"height:1px"}
            if !following(){button{class:"new-activity",onclick:move |_|following.set(true),"Follow new activity ↓"}}
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
    match kind {
        EventType::Message(message) => {
            rsx! {article{class:"message",div{class:"role","{message.message_type:?}" if provisional{" · streaming"}}pre{"{blocks(&message.content)}"}}}
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
            rsx! {details{class:"tool",summary{"Reasoning"}pre{"{serde_json::to_string_pretty(&thinking.contents).unwrap_or_default()}"}}}
        }
        EventType::Result(result) => {
            rsx! {details{class:"tool",summary{"Recorded result"}pre{"{serde_json::to_string_pretty(&result).unwrap_or_default()}"}}}
        }
        EventType::StatusChange(status) => rsx! {div{class:"muted","Agent {status.new_status:?}"}},
        _ => rsx! {},
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftCommandStage {
    Completion,
    Configurations,
    Models,
    SessionModels,
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

#[component]
fn DraftComposer(tab: Tab) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let target = tab.target;
    let session_id = target.session_id;
    let mut selected_index = use_signal(|| 0usize);
    let mut dismissed = use_signal(|| false);
    let mut choosing_model = use_signal(|| false);
    let mut all_models = use_signal(|| false);
    let mut textarea_node = use_signal(|| None::<std::rc::Rc<MountedData>>);
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
        None => 0,
    };
    rsx! { div { class:"composer",
        if let Some(stage) = stage { div { class:"popup command-popup", role:"listbox",
            "aria-label": match stage { DraftCommandStage::Completion => "Slash commands", DraftCommandStage::Configurations => "Agent configurations", DraftCommandStage::Models => "Root agent model policy", DraftCommandStage::SessionModels => "Session default model" },
            match stage {
                DraftCommandStage::Completion => rsx! {
                    for (index, command) in commands.iter().copied().enumerate() {
                        button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index,
                            onmousedown:move |event| event.prevent_default(),
                            onclick:move |_| { if command.id == CommandId::Refresh { refresh_draft_configs(ui, session_id, target); } else { ui.write().drafts.insert(target, format!("/{} ", command.name)); } selected_index.set(0); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
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
                            button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index,
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
                        button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index,
                            onmousedown:move |event| event.prevent_default(),
                            onclick:move |_| { spawn(change_model(ui, session_id, ModelConfig { model:model.model.clone(), effort:choose_effort(&model, default_effort) })); ui.write().drafts.remove(&target); if let Some(node)=textarea_node.read().clone() { spawn(async move { let _ = node.set_focus(true).await; }); } },
                            span { class:"command-title", "{model.name}" }
                            span { class:"command-description", "{model.model.provider.as_str()}/{model.model.api_name}" }
                        }
                    }
                    p { class:"command-hint", "↑↓ to navigate · Enter to select · Esc to dismiss" }
                },
                DraftCommandStage::Models => rsx! {
                    strong { class:"command-heading", "Choose the root agent model policy" }
                    if models.is_empty() { p { class:"command-empty", "No model policies available." } }
                    for (index, model) in models.iter().cloned().enumerate() {
                        { let label = draft_model_label(&model); rsx! {
                            button { class:if selected_index()==index { "command-option selected" } else { "command-option" }, role:"option", "aria-selected":selected_index()==index,
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
                        Key::ArrowDown => { event.prevent_default(); if count > 0 { selected_index.set((selected_index()+1)%count); } }
                        Key::ArrowUp => { event.prevent_default(); if count > 0 { selected_index.set((selected_index()+count-1)%count); } }
                        Key::Escape => { event.prevent_default(); dismissed.set(true); choosing_model.set(false); }
                        Key::Tab if stage == DraftCommandStage::Completion && !event.modifiers().contains(Modifiers::SHIFT) => {
                            event.prevent_default(); if let Some(command)=key_commands.get(selected_index()) { ui.write().drafts.insert(target,if command.id == CommandId::Refresh { "/refresh".into() } else { format!("/{} ", command.name) }); selected_index.set(0); }
                        }
                        Key::Enter if !event.modifiers().contains(Modifiers::SHIFT) => {
                            event.prevent_default();
                            match stage {
                                DraftCommandStage::Completion => if let Some(command)=key_commands.get(selected_index()) { if command.id == CommandId::Refresh { refresh_draft_configs(ui,session_id,target); } else { ui.write().drafts.insert(target,format!("/{} ", command.name)); } selected_index.set(0); }
                                DraftCommandStage::Configurations => if let Some(config)=key_configs.get(selected_index()) {
                                    choosing_model.set(select_draft_config(ui,session_id,target,config.config.config_id.clone())); selected_index.set(0);
                                },
                                DraftCommandStage::SessionModels => if let Some(model)=key_session_models.get(selected_index()) {
                                    spawn(change_model(ui,session_id,ModelConfig { model:model.model.clone(), effort:choose_effort(model,default_effort) })); ui.write().drafts.remove(&target); selected_index.set(0);
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
                    else { spawn(network::submit_draft_root(ui,session_id)); }
                }
            }
        }
          }
          button { class:"primary composer-send", "aria-label":"Send first message", disabled:blocked, onclick:move |_| { spawn(network::submit_draft_root(ui,session_id)); }, "↑" }
        }
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
    let mut dismissed = use_signal(|| false);
    let state = ui.read();
    let target = tab.target;
    let draft = state.drafts.get(&target).cloned().unwrap_or_default();
    let uncertain = state.uncertain_submissions.contains(&target);
    let parsed = parse_composer_input(&draft);
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
        .filter(|c| c.id == CommandId::SessionModel)
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
    let command = !dismissed() && (model_command || !commands.is_empty());
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
    let efforts: Vec<_> = current
        .as_ref()
        .and_then(|c| state.models.iter().find(|m| m.model == c.model))
        .map(|m| m.reasoning_options.iter().map(|o| o.effort).collect())
        .unwrap_or_else(|| vec![default_effort]);
    rsx! {div{class:"composer",
        if command{div{class:"popup command-popup",role:"listbox","aria-label":"Slash commands",
            if !model_command{
                div{class:"command-heading","Commands"}
                button{class:"command-option selected",role:"option","aria-selected":"true",onclick:move |_|{ui.write().drafts.insert(target,"/model ".into());selected.set(0);},
                    span{class:"command-title","/model"} span{class:"command-description","Change the session default model and effort"}
                }
                div{class:"command-hint","↑↓ navigate · Enter select · Tab complete · Esc dismiss"}
            }
            else{
                div{class:"command-heading","Session default model (not individual agents)"}
                if let Some(config)=&current{p{class:"muted","Current: {config.model.provider.as_str()}/{config.model.api_name} · {config.effort.as_str()}"}}
                button{onclick:move |_|{all.set(!all());selected.set(0);},if all(){"Show favorites"}else{"All models…"}}
                if models.is_empty(){p{class:"command-empty",if filter.is_empty(){"No favorite models. Open settings to pin models, or choose All models."}else{"No matching models. Try another search or choose All models."}}}
                for (index,model) in models.iter().cloned().enumerate(){
                    button{class:if selected()==index{"command-option selected"}else{"command-option"},role:"option","aria-selected":(selected()==index).to_string(),onclick:move |_|{spawn(change_model(ui,target.session_id,ModelConfig{model:model.model.clone(),effort:choose_effort(&model,default_effort)}));ui.write().drafts.remove(&target);},
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
                        Key::ArrowDown=>{event.prevent_default();let count=if model_command{key_models.len()}else{1};selected.set((selected()+1)%count.max(1));},
                        Key::ArrowUp=>{event.prevent_default();let count=if model_command{key_models.len()}else{1};selected.set((selected()+count.max(1)-1)%count.max(1));},
                        Key::Escape=>{event.prevent_default();dismissed.set(true);},
                        Key::Tab if !model_command && !event.modifiers().contains(Modifiers::SHIFT)=>{event.prevent_default();ui.write().drafts.insert(target,"/model ".into());selected.set(0);},
                        Key::Enter if !event.modifiers().contains(Modifiers::SHIFT)=>{event.prevent_default();if model_command{if let Some(model)=key_models.get(selected()){spawn(change_model(ui,target.session_id,ModelConfig{model:model.model.clone(),effort:choose_effort(model,default_effort)}));ui.write().drafts.remove(&target);}}else{ui.write().drafts.insert(target,"/model ".into());selected.set(0);}},
                        _=>{},
                    }
                } else if event.key()==Key::Enter&&!event.modifiers().contains(Modifiers::SHIFT){event.prevent_default();ui.write().workspace.promote(tab.id);spawn(network::submit(ui,target,false));}
            }
        }
          }
          button{class:"primary composer-send","aria-label":"Send steer",disabled:uncertain || matches!(parsed, ComposerInput::Command { .. }),onclick:move |_|{ui.write().workspace.promote(tab.id);spawn(network::submit(ui,target,false));},"↑"}
        }
        div{class:"composer-actions",
            if uncertain { button { onclick:move |_| { ui.write().uncertain_submissions.remove(&target); }, "I checked delivery; enable retry" } }
            button{onclick:move |_|{spawn(network::submit(ui,target,true));},"Interrupt"}
            span{class:"muted",{state.transcript_status.get(&target).map(String::as_str).unwrap_or("Loading…")}}
        }
        if let Some(config)=current{
            div{class:"composer-actions muted",span{"Session default: {config.model.api_name}"}
                select{"aria-label":"Session default reasoning effort",value:config.effort.as_str(),onchange:move |e|{let effort=match e.value().as_str(){"off"=>Effort::Off,"minimal"=>Effort::Minimal,"low"=>Effort::Low,"high"=>Effort::High,"xhigh"=>Effort::Xhigh,"max"=>Effort::Max,_=>Effort::Medium};spawn(change_model(ui,target.session_id,ModelConfig{model:config.model.clone(),effort}));},
                    for effort in efforts{option{value:effort.as_str(),"{effort.as_str()}"}}
                }
            }
        }
    }}
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
    let models: Vec<_> = state
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
            button { class:if tab() == "models" { "active" } else { "" }, "aria-current":if tab() == "models" { "page" } else { "false" }, onclick:move |_|tab.set("models".into()), "Model favorites" }
            button { class:if tab() == "appearance" { "active" } else { "" }, "aria-current":if tab() == "appearance" { "page" } else { "false" }, onclick:move |_|tab.set("appearance".into()), "Appearance" }
        }
        div { class:if tab() == "models" { "settings-content models-settings" } else { "settings-content" },
        if tab() == "profiles" { ProfileSettings { key:"{profile_id}" } }
        if tab() == "models" {
            h3{"Model favorites"}p{class:"muted","Pins are stored on this server and shared with its other clients."}
            button{onclick:move |_|{spawn(network::refresh_metadata(ui));},"Refresh probes and model catalog"}
            input{placeholder:"Filter models",value:filter(),oninput:move |event|filter.set(event.value()),"aria-label":"Filter model favorites"}
            div{class:"models",for model in models{div{class:"model-row",
                div{strong{"{model.name}"}div{class:"muted","{model.model.provider.as_str()}/{model.model.api_name}"}}
                button{"aria-label":if model.pinned{"Unpin model"}else{"Pin model"},onclick:move |_|{spawn(network::pin_model(ui,model.clone()));},if model.pinned{"★"}else{"☆"}}
            }}}
        }
        if tab() == "appearance" {
            h3{"Appearance"}
            label{r#for:"transcript-font-size","Transcript font size"}input{id:"transcript-font-size",r#type:"range",min:"11",max:"22",value:state.settings.preferences.transcript_font_size.to_string(),oninput:move |e|{if let Ok(size)=e.value().parse(){ui.write().settings.preferences.transcript_font_size=size;}}}
        }
        }
        }
    }}}
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
