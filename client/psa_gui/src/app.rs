use crate::{
    GuiBootstrap, network,
    reactive::{Shared, use_ui_selector, use_ui_subscription},
    style, views,
};
use client_shared::*;
use dioxus::{core::Task, document, prelude::*};
use psa::api::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[cfg(test)]
mod focus_tests;
#[cfg(test)]
mod shortcuts_tests;

pub const DRAFT_ROOT: AgentId = AgentId(0);

#[derive(Clone, Default)]
pub(crate) struct ModalRegistry(Rc<RefCell<Vec<(Rc<()>, Signal<bool>)>>>);

impl ModalRegistry {
    pub(crate) fn is_open(&self) -> bool {
        self.0.borrow().iter().any(|(_, open)| *open.peek())
    }

    /// Number of open modals registered before this one; the stack position.
    pub(crate) fn depth(&self, token: &Rc<()>) -> usize {
        self.0
            .borrow()
            .iter()
            .take_while(|(id, _)| !Rc::ptr_eq(id, token))
            .filter(|(_, open)| *open.peek())
            .count()
    }

    /// Whether this is the most recently opened modal still open.
    pub(crate) fn is_top(&self, token: &Rc<()>) -> bool {
        self.0
            .borrow()
            .iter()
            .rev()
            .find(|(_, open)| *open.peek())
            .is_none_or(|(id, _)| Rc::ptr_eq(id, token))
    }
}

/// Register a modal's open flag. Registration order is the stack order, so a
/// modal opened from inside another one stacks above it.
pub(crate) fn register_modal(open: Signal<bool>) -> Rc<()> {
    let registry = dioxus::core::try_consume_context::<ModalRegistry>();
    let token = use_hook(|| Rc::new(()));
    use_hook(|| {
        if let Some(registry) = &registry {
            registry.0.borrow_mut().push((token.clone(), open));
        }
    });
    let drop_token = token.clone();
    use_drop(move || {
        if let Some(registry) = registry {
            registry
                .0
                .borrow_mut()
                .retain(|(id, _)| !Rc::ptr_eq(id, &drop_token));
        }
    });
    token
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
            request_composer_focus(ui);
        }
        WorkspaceShortcut::Direction(direction) => {
            ui.write().workspace.focus_direction(direction);
            request_composer_focus(ui);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ComposerFocusIntent(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComposerFocusRequest {
    group: GroupId,
    view: ViewId,
    intent: Option<ComposerFocusIntent>,
}

pub(crate) fn begin_composer_open() -> ComposerFocusIntent {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_INTENT: AtomicU64 = AtomicU64::new(1);
    let intent = ComposerFocusIntent(NEXT_INTENT.fetch_add(1, Ordering::Relaxed));
    document::eval(&format!("window.psaBeginComposerOpen?.({});", intent.0));
    intent
}

fn focus_composer(request: ComposerFocusRequest) {
    let intent = request
        .intent
        .map(|intent| format!(", {}", intent.0))
        .unwrap_or_default();
    document::eval(&format!(
        "window.psaFocusComposer?.({}, {}{intent});",
        request.group.0, request.view.0
    ));
}

pub(crate) fn request_composer_focus(ui: Signal<Ui>) {
    let state = ui.peek();
    if let Some(tab) = state.workspace.active_tab() {
        focus_composer(ComposerFocusRequest {
            group: state.workspace.focused_group,
            view: tab.id,
            intent: None,
        });
    }
}

fn use_open_composer_focus(ui: Signal<Ui>) {
    let requests = use_ui_selector(|state| (state.generation, state.composer_focus_request));
    use_effect(move || {
        let Some(request) = requests.read().1 else {
            return;
        };
        let state = ui.peek();
        if state.workspace.focused_group == request.group
            && state
                .workspace
                .active_tab()
                .is_some_and(|tab| tab.id == request.view)
        {
            focus_composer(request);
        }
    });
}

#[derive(Clone, Default)]
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
        let has_default = self.configs.iter().any(|c| {
            Some(&c.config.config_id) == self.selected.as_ref() && c.config.first_model_default
        });
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
    pub settings: Shared<DesktopSettings>,
    pub workspace: Shared<Workspace>,
    pub sidebar: SidebarState,
    /// Session list criteria; probe and project selections are persisted.
    pub session_filter: SessionFilter,
    pub creation_target: Option<crate::creation::CreationTarget>,
    pub creation_revision: u64,
    pub creation_name: String,
    pub creation_pending: bool,
    pub creation_error: Option<String>,
    pub creation_uncertain: bool,
    pub last_agents: Shared<HashMap<SessionId, AgentId>>,
    pub sessions: Shared<Vec<Session>>,
    pub fresh_sessions: Shared<HashSet<SessionId>>,
    pub probes: Shared<Vec<Probe>>,
    pub probe_error: Option<String>,
    pub sandboxes: Shared<Vec<Sandbox>>,
    /// Projects of sessions on the server, refreshed on connect and when filtering.
    pub projects: Shared<Vec<ProjectId>>,
    /// Sandboxes with a lifecycle request in flight.
    pub sandbox_pending: HashSet<ProbeId>,
    pub agents: Shared<HashMap<SessionId, Shared<Vec<Agent>>>>,
    /// Chat models from the catalog; the only kind agents can run.
    pub models: Shared<Vec<ModelInfo>>,
    /// Decision models from the catalog, called from CodeMode.
    pub decision_models: Shared<Vec<ModelInfo>>,
    pub transcripts: HashMap<TranscriptTarget, Shared<Transcript>>,
    pub transcript_status: HashMap<TranscriptTarget, String>,
    pub history_requests: HashMap<TranscriptTarget, u64>,
    pub drafts: crate::input::DraftStore,
    pub uncertain_submissions: HashSet<TranscriptTarget>,
    pub pending_submissions: HashSet<TranscriptTarget>,
    pub pending_interrupts: HashSet<TranscriptTarget>,
    pub draft_roots: HashMap<SessionId, Shared<DraftRoot>>,
    pub network: RefCell<network::NetworkState>,
    pub next_sessions: Option<String>,
    pub generation: u64,
    pub connection: String,
    pub settings_open: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub follow_request: (u64, Option<ViewId>),
    /// A linked message to scroll to once the target transcript has loaded.
    pub pending_reveals: HashMap<TranscriptTarget, MessageLink>,
    composer_focus_request: Option<ComposerFocusRequest>,
}
impl Ui {
    pub fn new(settings: DesktopSettings) -> Self {
        let saved = settings.current_server().cloned().unwrap_or_default();
        let mut workspace = saved.workspace;
        workspace.restore();
        Self {
            settings: settings.into(),
            workspace: workspace.into(),
            sidebar: saved.sidebar,
            session_filter: saved.session_filter,
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
                .collect::<HashMap<_, _>>()
                .into(),
            sessions: Shared::default(),
            fresh_sessions: Shared::default(),
            probes: Shared::default(),
            probe_error: None,
            projects: Shared::default(),
            sandboxes: Shared::default(),
            sandbox_pending: HashSet::new(),
            agents: Shared::default(),
            models: Shared::default(),
            decision_models: Shared::default(),
            transcripts: HashMap::new(),
            transcript_status: HashMap::new(),
            history_requests: HashMap::new(),
            drafts: crate::input::DraftStore::default(),
            uncertain_submissions: HashSet::new(),
            pending_submissions: HashSet::new(),
            pending_interrupts: HashSet::new(),
            draft_roots: HashMap::new(),
            network: RefCell::default(),
            next_sessions: None,
            generation: 0,
            connection: "Not connected".into(),
            settings_open: false,
            error: None,
            notice: None,
            follow_request: (0, None),
            pending_reveals: HashMap::new(),
            composer_focus_request: None,
        }
    }
    pub fn persisted(&self) -> DesktopSettings {
        let mut settings = (*self.settings).clone();
        let saved = settings.ensure_current_server();
        saved.workspace = (*self.workspace).clone();
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
        saved.session_filter = SessionFilter {
            name_contains: String::new(),
            ..self.session_filter.clone()
        };
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
        self.open_with_focus(target, permanent, None);
    }
    fn open_with_focus(
        &mut self,
        target: TranscriptTarget,
        permanent: bool,
        intent: Option<ComposerFocusIntent>,
    ) {
        let existing = self
            .workspace
            .groups()
            .iter()
            .any(|group| group.tabs.iter().any(|tab| tab.target == target));
        let view = self.workspace.open(target, permanent);
        if !existing {
            self.composer_focus_request = Some(ComposerFocusRequest {
                group: self.workspace.focused_group,
                view,
                intent,
            });
        }
        if target.agent_id != DRAFT_ROOT {
            self.last_agents.insert(target.session_id, target.agent_id);
        }
    }
    #[cfg(test)]
    pub fn open_session_target(
        &mut self,
        target: TranscriptTarget,
        permanent: bool,
        from_double_click: bool,
    ) {
        self.open_session_target_with_focus(target, permanent, from_double_click, None);
    }
    /// Opens another agent's transcript, scrolled to a linked message.
    pub fn reveal_message(&mut self, target: TranscriptTarget, link: MessageLink) {
        self.pending_reveals.insert(target, link);
        self.open(target, true);
    }
    /// Show the makeup of the context sent for one of an agent's model turns.
    pub fn open_context(&mut self, target: TranscriptTarget, turn: u64) {
        self.workspace.open_context(target, turn);
    }
    pub(crate) fn open_session_target_with_focus(
        &mut self,
        target: TranscriptTarget,
        permanent: bool,
        from_double_click: bool,
        intent: Option<ComposerFocusIntent>,
    ) {
        self.open_with_focus(target, permanent, intent);
        if from_double_click && self.settings.preferences.double_click_session_opens_details {
            self.sidebar.expanded_mode = SidebarMode::Details;
            self.sidebar.collapsed = false;
        }
    }
    pub fn upsert_session(&mut self, session: Session) {
        if !self.fresh_sessions.contains(&session.id) {
            self.fresh_sessions.insert(session.id);
        }
        if let Some(old) = self.sessions.iter_mut().find(|s| s.id == session.id) {
            *old = session;
        } else {
            self.sessions.push(session);
        }
    }
    /// Loaded transcript payloads belong to open views, not the draft cache.
    /// Closing the final view releases its history; reopening bootstraps afresh.
    pub fn prune_closed_transcripts(&mut self, targets: &HashSet<TranscriptTarget>) {
        self.transcripts
            .retain(|target, _| targets.contains(target));
        self.transcript_status
            .retain(|target, _| targets.contains(target));
        self.history_requests
            .retain(|target, _| targets.contains(target));
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
    let mut ui = use_signal(move || Ui::new(initial));
    use_context_provider(|| ui);
    let modals = use_context_provider(ModalRegistry::default);
    // Key events targeted at the document (for example after the focused tab
    // is removed) do not bubble through the shell. Forward those to it.
    use_effect(|| {
        document::eval(include_str!("pane_focus.js"));
        document::eval(include_str!("wheel.js"));
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
            "window.removeEventListener('keydown', window.psaWorkspaceKeyForwarder, true); delete window.psaWorkspaceKeyForwarder; window.psaRemovePaneFocus?.(); window.psaRemoveHoveredWheel?.();",
        );
    });
    use_open_composer_focus(ui);
    let platform = bootstrap.platform.clone();
    let mut local_ui = ui;
    let generation = use_memo(move || ui.read().generation);
    let browser_generation = use_hook(|| Rc::new(std::cell::Cell::new(*generation.peek())));
    use_effect(move || {
        let current = generation();
        let previous = browser_generation.replace(current);
        if previous != current {
            // Old profile editors can still be connected until this render's
            // DOM edits land. Do not discard drafts belonging to the new scope.
            document::eval(&format!(
                "requestAnimationFrame(() => requestAnimationFrame(() => window.psaInputs?.pruneDisconnectedGroups?.('psa:{previous}:')));"
            ));
        }
    });
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
    let open_views = use_ui_selector(|state| (state.generation, state.workspace.clone()));
    use_effect(move || {
        let selected = open_views.read();
        let generation = selected.0;
        let targets: HashSet<_> = selected
            .1
            .groups()
            .iter()
            .flat_map(|g| g.tabs.iter().map(|t| t.target))
            .collect();
        drop(selected);
        if ui
            .peek()
            .transcripts
            .keys()
            .any(|target| !targets.contains(target))
            || ui
                .peek()
                .transcript_status
                .keys()
                .any(|target| !targets.contains(target))
            || ui
                .peek()
                .history_requests
                .keys()
                .any(|target| !targets.contains(target))
        {
            ui.write().prune_closed_transcripts(&targets);
        }
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
    let durable = use_ui_selector(|state| {
        (
            state.settings.clone(),
            state.workspace.clone(),
            state.sidebar.clone(),
            state.session_filter.probe_ids.clone(),
            state.session_filter.project_ids.clone(),
            state.last_agents.clone(),
        )
    });
    let last_saved = use_hook(|| Rc::new(RefCell::new(None::<DesktopSettings>)));
    let platform = bootstrap.platform.clone();
    let mut save_ui = ui;
    let saver = use_hook(move || {
        let (handle, receiver) = crate::persistence::settings_save_channel();
        dioxus::core::spawn_forever(async move {
            let _ = receiver
                .run(
                    std::time::Duration::from_millis(200),
                    std::time::Duration::from_secs(1),
                    move |settings| {
                        let platform = platform.clone();
                        async move { platform.save_settings(settings).await }
                    },
                    move |_| {
                        if let Ok(mut state) = save_ui.try_write() {
                            state.error = Some("Unable to save desktop settings; retrying".into());
                        }
                    },
                )
                .await;
        });
        handle
    });
    let flush_platform = bootstrap.platform.clone();
    let flushing_saver = saver.clone();
    use_hook(move || {
        flush_platform.set_settings_flush(Some(Rc::new(move || {
            // A close can precede the durable-projection effect's next poll.
            flushing_saver.submit(ui.peek().persisted());
            let handle = flushing_saver.clone();
            Box::pin(async move { handle.flush().await })
        })));
    });
    let closing_saver = saver.clone();
    let closing_platform = bootstrap.platform.clone();
    use_drop(move || {
        closing_platform.set_settings_flush(None);
        dioxus::core::spawn_forever(async move {
            let _ = closing_saver.shutdown().await;
        });
    });
    use_effect(move || {
        let _ = durable.read();
        let settings = ui.peek().persisted();
        if last_saved.borrow().as_ref() == Some(&settings) {
            return;
        }
        *last_saved.borrow_mut() = Some(settings.clone());
        saver.submit(settings);
    });
    use_ui_subscription(|state| {
        (
            state.generation,
            state.workspace.clone(),
            state.sidebar.collapsed,
            state.sidebar.width,
            state.settings_open,
        )
    });
    // One browser controller drives tab dragging for every pane.
    use_hook(move || {
        let mut ui = ui;
        spawn(async move {
            let mut eval = document::eval(include_str!("tab_drag.js"));
            while let Ok(drop) = eval.recv::<views::TabDrop>().await {
                views::apply_tab_drop(&mut ui.write().workspace, drop);
            }
        });
    });
    let state = ui.peek();
    rsx! {
        style { "{style::CSS}" }
        div { id:"psa-shell", class:"shell", onkeydown: move |event| handle_workspace_key(ui, &modals, event),
            views::Rail {}
            if !state.sidebar.collapsed {
                views::Sidebar { key: "{state.generation}" }
                crate::interaction::EdgeResizer {
                    target_id: views::SIDEBAR_ID,
                    width: f64::from(state.sidebar.width()),
                    min: f64::from(SidebarState::MIN_WIDTH),
                    max: f64::from(SidebarState::MAX_WIDTH),
                    aria_label: "Resize sidebar",
                    oncommit: move |width: f64| ui.write().sidebar.set_width(width),
                }
            }
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
    let notifications = use_memo(move || {
        let state = ui.read();
        (
            state.connection.clone(),
            state.error.clone(),
            state.notice.clone(),
        )
    });
    let _ = notifications.read();
    let state = ui.peek();
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
