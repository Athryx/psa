use crate::{GuiBootstrap, network, style, views};
use client_shared::*;
use dioxus::{core::Task, document, prelude::*};
use futures_util::StreamExt;
use psa_api::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[cfg(test)]
mod shortcuts_tests;

pub const DRAFT_ROOT: AgentId = AgentId(0);

#[derive(Clone, Default)]
pub(crate) struct ModalRegistry(Rc<RefCell<Vec<(Rc<()>, Signal<bool>)>>>);

impl ModalRegistry {
    pub(crate) fn is_open(&self) -> bool {
        self.0.borrow().iter().any(|(_, open)| *open.peek())
    }
}

pub(crate) fn register_modal(open: Signal<bool>) {
    let registry = dioxus::core::try_consume_context::<ModalRegistry>();
    let token = use_hook(|| Rc::new(()));
    use_hook(|| {
        if let Some(registry) = &registry {
            registry.0.borrow_mut().push((token.clone(), open));
        }
    });
    use_drop(move || {
        if let Some(registry) = registry {
            registry
                .0
                .borrow_mut()
                .retain(|(id, _)| !Rc::ptr_eq(id, &token));
        }
    });
}

enum WorkspaceShortcut {
    Follow,
    Close,
    Tab(usize),
    Direction(FocusDirection),
}

fn workspace_shortcut(key: &Key, code: Code, modifiers: Modifiers) -> Option<WorkspaceShortcut> {
    if modifiers == Modifiers::CONTROL && *key == Key::End {
        return Some(WorkspaceShortcut::Follow);
    }
    if modifiers == Modifiers::CONTROL {
        return matches!(key, Key::Character(value) if value.eq_ignore_ascii_case("w"))
            .then_some(WorkspaceShortcut::Close);
    }
    if modifiers != Modifiers::ALT {
        return None;
    }
    let direction = match key {
        Key::ArrowLeft => Some(FocusDirection::Left),
        Key::ArrowRight => Some(FocusDirection::Right),
        Key::ArrowUp => Some(FocusDirection::Up),
        Key::ArrowDown => Some(FocusDirection::Down),
        _ => None,
    };
    if let Some(direction) = direction {
        return Some(WorkspaceShortcut::Direction(direction));
    }
    let index = match code {
        Code::Digit1 => 0,
        Code::Digit2 => 1,
        Code::Digit3 => 2,
        Code::Digit4 => 3,
        Code::Digit5 => 4,
        Code::Digit6 => 5,
        Code::Digit7 => 6,
        Code::Digit8 => 7,
        Code::Digit9 => 8,
        _ => return None,
    };
    Some(WorkspaceShortcut::Tab(index))
}

pub(crate) fn handle_workspace_key(
    mut ui: Signal<Ui>,
    modals: &ModalRegistry,
    event: KeyboardEvent,
) {
    let Some(shortcut) = workspace_shortcut(&event.key(), event.code(), event.modifiers()) else {
        return;
    };
    event.prevent_default();
    if ui.peek().settings_open || modals.is_open() {
        return;
    }
    match shortcut {
        WorkspaceShortcut::Follow => {
            let active = ui.peek().workspace.active_tab().map(|tab| tab.id);
            if let Some(view) = active {
                let mut state = ui.write();
                state.follow_request = (state.follow_request.0.wrapping_add(1), Some(view));
            }
        }
        WorkspaceShortcut::Close => {
            ui.write().workspace.close_active();
        }
        WorkspaceShortcut::Tab(index) => {
            ui.write().workspace.focus_tab_at(index);
        }
        WorkspaceShortcut::Direction(direction) => {
            ui.write().workspace.focus_direction(direction);
        }
    }
}

#[derive(Default)]
pub struct DraftRoot {
    pub configs: Vec<AgentConfigStatus>,
    pub selected: Option<ConfigId>,
    pub model: Option<ModelSelection>,
    pub loading: bool,
    pub ready: bool,
    pub pending: bool,
    pub uncertain: bool,
}

impl DraftRoot {
    /// Return one permitted wire selector for each distinct root model policy.
    pub fn model_choices(&self) -> Vec<ModelSelection> {
        let Some(config) = self
            .configs
            .iter()
            .find(|c| Some(&c.config.config_id) == self.selected.as_ref())
        else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for selection in &config.config.models {
            if !choices
                .iter()
                .any(|other| root_models_match(selection, other))
            {
                choices.push(selection.clone());
            }
        }
        choices
    }

    pub fn select_config(&mut self, id: ConfigId) {
        self.selected = Some(id);
        let choices = self.model_choices();
        let has_default = self
            .configs
            .iter()
            .any(|c| Some(&c.config.config_id) == self.selected.as_ref() && c.config.default);
        self.model = if has_default || choices.len() == 1 {
            choices.into_iter().next()
        } else {
            None
        };
    }
}

fn root_models_match(left: &ModelSelection, right: &ModelSelection) -> bool {
    match (left, right) {
        (
            ModelSelection::Global | ModelSelection::Parent,
            ModelSelection::Global | ModelSelection::Parent,
        ) => true,
        (
            ModelSelection::Fixed {
                model: left_model,
                effort: left_effort,
            },
            ModelSelection::Fixed {
                model: right_model,
                effort: right_effort,
            },
        ) => {
            left_model == right_model
                && (left_effort == right_effort
                    || matches!(
                        (left_effort, right_effort),
                        (
                            EffortSelection::Global | EffortSelection::Parent,
                            EffortSelection::Global | EffortSelection::Parent
                        )
                    ))
        }
        _ => false,
    }
}

pub struct Ui {
    pub settings: DesktopSettings,
    pub workspace: Workspace,
    pub sidebar: SidebarState,
    pub selected_probe: Option<ProbeId>,
    pub creation_target: Option<crate::creation::CreationTarget>,
    pub creation_revision: u64,
    pub creation_name: String,
    pub creation_pending: bool,
    pub creation_error: Option<String>,
    pub creation_uncertain: bool,
    pub last_agents: HashMap<SessionId, AgentId>,
    pub sessions: Vec<Session>,
    pub fresh_sessions: HashSet<SessionId>,
    pub probes: Vec<Probe>,
    pub probe_error: Option<String>,
    pub agents: HashMap<SessionId, Vec<Agent>>,
    pub models: Vec<ModelInfo>,
    pub transcripts: HashMap<TranscriptTarget, Transcript>,
    pub transcript_status: HashMap<TranscriptTarget, String>,
    pub drafts: HashMap<TranscriptTarget, String>,
    pub uncertain_submissions: HashSet<TranscriptTarget>,
    pub pending_submissions: HashSet<TranscriptTarget>,
    pub pending_interrupts: HashSet<TranscriptTarget>,
    pub draft_roots: HashMap<SessionId, DraftRoot>,
    pub revisions: HashMap<SessionId, u64>,
    pub next_sessions: Option<String>,
    pub generation: u64,
    pub selection_request: u64,
    pub list_request: u64,
    pub probe_request: u64,
    pub connection: String,
    pub settings_open: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub dragging: Option<ViewId>,
    pub follow_request: (u64, Option<ViewId>),
}
impl Ui {
    pub fn new(settings: DesktopSettings) -> Self {
        let saved = settings.current_server().cloned().unwrap_or_default();
        let mut workspace = saved.workspace;
        workspace.restore();
        Self {
            settings,
            workspace,
            sidebar: saved.sidebar,
            selected_probe: saved.selected_probe,
            creation_target: None,
            creation_revision: 0,
            creation_name: String::new(),
            creation_pending: false,
            creation_error: None,
            creation_uncertain: false,
            last_agents: saved
                .last_viewed_agents
                .into_iter()
                .map(|t| (t.session_id, t.agent_id))
                .collect(),
            sessions: Vec::new(),
            fresh_sessions: HashSet::new(),
            probes: Vec::new(),
            probe_error: None,
            agents: HashMap::new(),
            models: Vec::new(),
            transcripts: HashMap::new(),
            transcript_status: HashMap::new(),
            drafts: HashMap::new(),
            uncertain_submissions: HashSet::new(),
            pending_submissions: HashSet::new(),
            pending_interrupts: HashSet::new(),
            draft_roots: HashMap::new(),
            revisions: HashMap::new(),
            next_sessions: None,
            generation: 0,
            selection_request: 0,
            list_request: 0,
            probe_request: 0,
            connection: "Not connected".into(),
            settings_open: false,
            error: None,
            notice: None,
            dragging: None,
            follow_request: (0, None),
        }
    }
    pub fn persisted(&self) -> DesktopSettings {
        let mut settings = self.settings.clone();
        let saved = settings.ensure_current_server();
        saved.workspace = self.workspace.clone();
        let draft_tabs: Vec<_> = saved
            .workspace
            .groups()
            .iter()
            .flat_map(|g| g.tabs.iter())
            .filter(|t| t.target.agent_id == DRAFT_ROOT)
            .map(|t| t.id)
            .collect();
        for id in draft_tabs {
            saved.workspace.close(id);
        }
        saved.sidebar = self.sidebar.clone();
        saved.selected_probe = self.selected_probe;
        saved.last_viewed_agents = self
            .last_agents
            .iter()
            .map(|(&session_id, &agent_id)| TranscriptTarget {
                session_id,
                agent_id,
            })
            .collect();
        saved.last_viewed_agents.sort_by_key(|t| t.session_id.0);
        settings
    }
    pub fn open(&mut self, target: TranscriptTarget, permanent: bool) {
        self.workspace.open(target, permanent);
        if target.agent_id != DRAFT_ROOT {
            self.last_agents.insert(target.session_id, target.agent_id);
        }
    }
    pub fn open_session_target(
        &mut self,
        target: TranscriptTarget,
        permanent: bool,
        from_double_click: bool,
    ) {
        self.open(target, permanent);
        if from_double_click && self.settings.preferences.double_click_session_opens_details {
            self.sidebar.expanded_mode = SidebarMode::Details;
            self.sidebar.collapsed = false;
        }
    }
    pub fn upsert_session(&mut self, session: Session) {
        self.fresh_sessions.insert(session.id);
        if let Some(old) = self.sessions.iter_mut().find(|s| s.id == session.id) {
            *old = session;
        } else {
            self.sessions.push(session);
        }
    }
    pub fn change_connection(&mut self, url: String, key: String) {
        let mut settings = self.persisted();
        let profile = settings.active_profile_mut();
        profile.connection.api_url = url;
        profile.connection.client_key = key;
        let generation = self.generation + 1;
        let settings_open = self.settings_open;
        *self = Self::new(settings);
        self.generation = generation;
        self.settings_open = settings_open;
    }

    pub fn select_profile(&mut self, id: String) {
        if self.settings.active_profile_id == id
            || !self.settings.profiles.iter().any(|p| p.id == id)
        {
            return;
        }
        let mut settings = self.persisted();
        settings.active_profile_id = id;
        let generation = self.generation + 1;
        *self = Self::new(settings);
        self.generation = generation;
        self.settings_open = true;
    }
}

pub fn app() -> Element {
    let bootstrap = use_context::<GuiBootstrap>();
    let initial = bootstrap.config.settings.clone();
    let ui = use_signal(move || Ui::new(initial));
    use_context_provider(|| ui);
    let modals = use_context_provider(ModalRegistry::default);
    // Key events targeted at the document (for example after the focused tab
    // is removed) do not bubble through the shell. Forward those to it.
    use_effect(|| {
        document::eval(
            r#"
            if (!window.psaWorkspaceKeyForwarder) {
                window.psaWorkspaceKeyForwarder = event => {
                    const shell = document.getElementById('psa-shell');
                    if (!shell || shell.contains(event.target)) return;
                    const close = event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && event.key.toLowerCase() === 'w';
                    const navigate = event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey && (/^Digit[1-9]$/.test(event.code) || /^Arrow(Left|Right|Up|Down)$/.test(event.key));
                    const follow = event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && event.key === 'End';
                    if (!close && !navigate && !follow) return;
                    event.preventDefault();
                    shell.dispatchEvent(new KeyboardEvent('keydown', { key: event.key, code: event.code, ctrlKey: event.ctrlKey, altKey: event.altKey, metaKey: event.metaKey, shiftKey: event.shiftKey, bubbles: true, cancelable: true }));
                };
                window.addEventListener('keydown', window.psaWorkspaceKeyForwarder, true);
            }
        "#,
        );
    });
    use_drop(|| {
        document::eval(
            "window.removeEventListener('keydown', window.psaWorkspaceKeyForwarder, true); delete window.psaWorkspaceKeyForwarder;",
        );
    });
    let platform = bootstrap.platform.clone();
    let mut local_ui = ui;
    let generation = use_memo(move || ui.read().generation);
    use_effect(move || {
        let selected_generation = generation();
        if local_ui.peek().settings.active_profile_id != LOCAL_PROFILE_ID {
            return;
        }
        let platform = platform.clone();
        spawn(async move {
            let status = platform.local_server_status().await;
            let mut started = false;
            let status = match status {
                Ok(LocalServerStatus::Stopped)
                    if local_ui.read().generation == selected_generation
                        && local_ui.read().settings.active_profile_id == LOCAL_PROFILE_ID =>
                {
                    started = true;
                    platform.local_server_start().await
                }
                other => other,
            };
            if let Err(error) = &status {
                if local_ui.read().generation == selected_generation
                    && local_ui.read().settings.active_profile_id == LOCAL_PROFILE_ID
                {
                    local_ui.write().error = Some(format!("Unable to start local server: {error}"));
                }
            }
            if let Ok(LocalServerStatus::Running { connection }) = status {
                let state = local_ui.read();
                let active = state.generation == selected_generation
                    && state.settings.active_profile_id == LOCAL_PROFILE_ID;
                let changed = state.settings.active_profile().connection.api_url
                    != connection.api_url
                    || state.settings.active_profile().connection.client_key
                        != connection.client_key;
                drop(state);
                if active && (changed || started) {
                    local_ui
                        .write()
                        .change_connection(connection.api_url, connection.client_key);
                }
            }
        });
    });
    let summary_task = use_hook(|| Rc::new(RefCell::new(None::<Task>)));
    use_effect(move || {
        let generation = generation();
        if let Some(task) = summary_task.borrow_mut().take() {
            task.cancel();
        }
        *summary_task.borrow_mut() = Some(spawn(network::summaries(ui, generation)));
    });
    // Dioxus tasks are owned by this component. Closing the final view cancels
    // its stream; server switches cancel all old subscriptions immediately.
    let tasks = use_hook(|| {
        Rc::new(RefCell::new(HashMap::<
            SessionId,
            (Vec<TranscriptTarget>, Task),
        >::new()))
    });
    let task_generation = use_hook(|| Rc::new(RefCell::new(0u64)));
    use_effect(move || {
        let state = ui.read();
        let generation = state.generation;
        let targets: HashSet<_> = state
            .workspace
            .groups()
            .iter()
            .flat_map(|g| g.tabs.iter().map(|t| t.target))
            .collect();
        let mut sessions = HashMap::<SessionId, Vec<TranscriptTarget>>::new();
        for target in targets {
            if target.agent_id != DRAFT_ROOT {
                sessions.entry(target.session_id).or_default().push(target);
            }
        }
        for targets in sessions.values_mut() {
            targets.sort_by_key(|t| t.agent_id.0);
        }
        let mut tasks = tasks.borrow_mut();
        if *task_generation.borrow() != generation {
            for (_, (_, task)) in tasks.drain() {
                task.cancel();
            }
            *task_generation.borrow_mut() = generation;
        }
        tasks.retain(|session, (targets, task)| {
            if sessions.get(session) == Some(targets) {
                true
            } else {
                task.cancel();
                false
            }
        });
        for (session, targets) in sessions {
            tasks.entry(session).or_insert_with(|| {
                (
                    targets.clone(),
                    spawn(network::session(ui, session, targets)),
                )
            });
        }
    });
    let last_saved = use_hook(|| Rc::new(RefCell::new(String::new())));
    let platform = bootstrap.platform.clone();
    let mut save_ui = ui;
    let saver = use_coroutine(move |mut requests: UnboundedReceiver<DesktopSettings>| {
        let platform = platform.clone();
        async move {
            while let Some(settings) = requests.next().await {
                if platform.save_settings(settings).await.is_err() {
                    save_ui.write().error = Some("Unable to save desktop settings".into());
                }
            }
        }
    });
    use_effect(move || {
        let settings = ui.read().persisted();
        let encoded = serde_json::to_string(&settings).unwrap_or_default();
        if *last_saved.borrow() == encoded {
            return;
        }
        *last_saved.borrow_mut() = encoded;
        saver.send(settings);
    });
    let state = ui.read();
    rsx! {
        style { "{style::CSS}" }
        div { id:"psa-shell", class:"shell", onkeydown: move |event| handle_workspace_key(ui, &modals, event),
            views::Rail {}
            if !state.sidebar.collapsed { views::Sidebar { key: "{state.generation}" } }
            main { class:"workspace",
                StatusBar { ui }
                div { class:"layout", views::Layout {key:"{state.generation}",node:state.workspace.root.clone(),path:Vec::new()} }
            }
            if state.settings_open { views::Settings {} }
        }
    }
}

#[component]
pub(crate) fn StatusBar(mut ui: Signal<Ui>) -> Element {
    let state = ui.read();
    rsx! {
        div { class:"connection",
            span { class:"connection-label", title:"{state.connection}", "{state.connection}" }
            span { class:"workspace-label", "PSA · workspace" }
            div { class:"status-notifications",
                div { class:"status-live", role:"alert",
                    if let Some(error) = &state.error {
                        details { class:"status-notification status-error",
                            summary { title:"{error}", "Error: {error}" }
                            div { class:"status-popover",
                                p { "{error}" }
                                button { "aria-label":"Dismiss error", onclick:move |_|ui.write().error=None, "Dismiss" }
                            }
                        }
                    }
                }
                div { class:"status-live", role:"status",
                    if let Some(notice) = &state.notice {
                        details { class:"status-notification",
                            summary { title:"{notice}", "Notice: {notice}" }
                            div { class:"status-popover",
                                p { "{notice}" }
                                button { "aria-label":"Dismiss notice", onclick:move |_|ui.write().notice=None, "Dismiss" }
                            }
                        }
                    }
                }
            }
        }
    }
}
