//! Target-scoped composer UI and browser-owned editing.
use crate::{
    app::Ui,
    command_prompt::CommandPrompt,
    input::{BrowserInput, DraftStore, InputKeyEvent, InputSnapshot},
    network,
    reactive::{use_ui_selector, use_ui_subscription},
};
use client_shared::*;
use dioxus::{
    core::{AttributeValue, spawn_forever},
    prelude::*,
};
use llm_api::providers::{Effort, Model, ModelConfig};
use psa::api::*;

#[cfg(test)]
use crate::transcript_view::TranscriptView;

/// The command whose arguments `text` is typing, such as `/model` for
/// `/model gpt`.
fn argument_command(text: &str) -> Option<CommandId> {
    let (name, _) = text.strip_prefix('/')?.split_once(char::is_whitespace)?;
    find_command(name, Frontend::Gui).map(|spec| spec.id)
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

/// A new session's composer also reloads its root-agent choices; elsewhere
/// `/refresh` only re-runs the server's configuration discovery.
fn refresh_configs(mut ui: Signal<Ui>, target: TranscriptTarget) {
    if target.agent_id == crate::app::DRAFT_ROOT {
        refresh_draft_configs(ui, target.session_id, target);
        return;
    }
    ui.write().drafts.remove(&target);
    spawn(network::record_command(
        ui,
        target,
        CommandId::Refresh,
        String::new(),
        network::refresh_configs(ui, target.session_id),
    ));
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
#[path = "tests/composer/draft_composer_tests.rs"]
mod draft_composer_tests;

#[cfg(test)]
#[path = "tests/composer/status_tests.rs"]
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

/// Catalog display name for a model, falling back to its API model name.
pub(crate) fn model_display_name(models: &[ModelInfo], model: &Model) -> String {
    models
        .iter()
        .find(|info| info.model == *model)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| model.model_name().to_owned())
}

/// Model name and color-coded effort on one line, as in the composer status.
#[component]
pub(crate) fn ModelEffort(model: String, effort: Effort) -> Element {
    rsx! {
        span { class:"composer-status-model", "{model}" }
        " · "
        span { class:"effort-{effort.as_str()}", "{effort.as_str()}" }
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
            (Some(model.clone()), effort)
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

fn select_session_effort(mut ui: Signal<Ui>, target: TranscriptTarget, effort: Effort) {
    let config = {
        let state = ui.peek();
        let current = state
            .sessions
            .iter()
            .find(|session| session.id == target.session_id)
            .map(|session| &session.model_config);
        build_effort_config(&state.models, current, effort)
    };
    if let Some(config) = config {
        let generation = ui.peek().generation;
        let change = async move {
            if ui.peek().generation != generation {
                return None;
            }
            change_model(ui, target.session_id, config).await
        };
        spawn(network::record_command(
            ui,
            target,
            CommandId::SessionEffort,
            effort.as_str().into(),
            change,
        ));
        ui.write().drafts.remove(&target);
    }
}

/// `/rename`, recorded in the transcript of the agent it was typed to.
fn spawn_rename(ui: Signal<Ui>, target: TranscriptTarget, name: String) {
    spawn(network::record_command(
        ui,
        target,
        CommandId::Rename,
        name.clone(),
        rename_session(ui, target.session_id, name),
    ));
}

async fn rename_session(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    name: String,
) -> network::CommandEnd {
    let generation = ui.peek().generation;
    let Ok(api) = network::client(ui) else {
        return None;
    };
    let result = api
        .update_session(
            session_id,
            &UpdateSession {
                name: Some(name),
                ..Default::default()
            },
        )
        .await;
    if generation != ui.peek().generation {
        return None;
    }
    match result {
        Ok(_) => {
            ui.write().notice = Some("Session renamed.".into());
            Some(Ok(()))
        }
        Err(error) => {
            ui.write().error = Some("Unable to rename the session".into());
            Some(Err(format!("Unable to rename the session: {error}")))
        }
    }
}

/// A slash command waiting for a value in the prompt above the composer line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashPrompt {
    RenameSession,
}

fn focus_node(node: Signal<Option<std::rc::Rc<MountedData>>>) {
    if let Some(node) = node.peek().clone() {
        spawn(async move {
            let _ = node.set_focus(true).await;
        });
    }
}

async fn change_model(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    model_config: ModelConfig,
) -> network::CommandEnd {
    let generation = ui.peek().generation;
    let Ok(api) = network::client(ui) else {
        return None;
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
    if generation != ui.peek().generation {
        return None;
    }
    match result {
        Ok(_) => {
            ui.write().notice =
                Some("Session default updated. Pinned agents are unchanged.".into());
            Some(Ok(()))
        }
        Err(error) => {
            ui.write().error = Some("Unable to change the session default model/effort".into());
            Some(Err(format!(
                "Unable to change the session default model/effort: {error}"
            )))
        }
    }
}

/// The shared menu, and whether it is `/agent`'s model policy step.
struct ComposerMenu {
    menu: CommandMenu,
    root_model: bool,
}

/// The composer's own menu state, kept per editor.
#[derive(Clone, Copy)]
struct ComposerState {
    selected: Signal<usize>,
    /// Menu state expires on a shared browser edit, not intentional replacements.
    dismissed: Signal<Option<u64>>,
    choosing_model: Signal<Option<u64>>,
    all_models: Signal<bool>,
    info_open: Signal<bool>,
    prompt: Signal<Option<SlashPrompt>>,
}

fn command_context(root: bool) -> CommandContext {
    if root {
        CommandContext::DraftRoot
    } else {
        CommandContext::Agent
    }
}

/// Both rendering and key handling use this builder. In particular, key handling
/// calls it after accepting the browser's key-time snapshot, never a render's list.
fn build_command_menu(
    state: &Ui,
    target: TranscriptTarget,
    text: &str,
    root: bool,
    dismissed: bool,
    choosing_model: bool,
    all_models: bool,
) -> Option<ComposerMenu> {
    if dismissed {
        return None;
    }
    let draft = state
        .draft_roots
        .get(&target.session_id)
        .filter(|_| root)
        .map(|draft| &**draft);
    if let Some(draft) = draft.filter(|_| choosing_model) {
        return Some(ComposerMenu {
            menu: build_root_model_menu(draft),
            root_model: true,
        });
    }

    let current = state
        .sessions
        .iter()
        .find(|session| session.id == target.session_id)
        .map(|session| &session.model_config);
    let context = MenuContext {
        frontend: Frontend::Gui,
        context: command_context(root),
        models: &state.models,
        session_model: current,
        show_all_models: all_models,
        local_server: state.settings.active_profile_id == LOCAL_PROFILE_ID,
        root: draft,
        probes: &state.probes,
    };
    let menu = client_shared::build_command_menu(text, &context)?;
    #[cfg(test)]
    if menu.command == Some(CommandId::SessionModel) {
        composer_tests::record_work(target, 2);
    }
    Some(ComposerMenu {
        menu,
        root_model: false,
    })
}

/// Run a menu choice. The menu's highlight is reset, except that a toggle
/// stays highlighted in the list it changed.
fn apply_menu_choice(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    root: bool,
    choice: MenuAction,
    mut composer: ComposerState,
) {
    composer.selected.set(0);
    if !matches!(choice, MenuAction::ToggleAllModels) {
        composer.all_models.set(false);
    }
    match choice {
        MenuAction::ChooseConfig(id) => {
            let choose = select_draft_config(ui, target.session_id, target, id);
            let clock = ui
                .peek()
                .drafts
                .snapshot(&target)
                .map(|snapshot| snapshot.revision.clock);
            composer
                .choosing_model
                .set(if choose { clock } else { None });
        }
        MenuAction::ChooseRootModel(model) => {
            if let Some(draft) = ui.write().draft_roots.get_mut(&target.session_id) {
                draft.model = Some(model);
            }
            composer.choosing_model.set(None);
        }
        MenuAction::Fill(text) => {
            ui.write().drafts.insert(target, text);
        }
        MenuAction::ToggleAllModels => {
            let all = !*composer.all_models.peek();
            composer.all_models.set(all);
            let text = ui.peek().drafts.get(&target).cloned().unwrap_or_default();
            let menu = build_command_menu(&ui.peek(), target, &text, root, false, false, all);
            let toggle = menu.and_then(|menu| {
                menu.menu
                    .candidates
                    .iter()
                    .position(|option| option.kind == CandidateKind::Toggle)
            });
            composer.selected.set(toggle.unwrap_or(0));
        }
        MenuAction::SetModel(config) => {
            let args = format!("{} {}", config.model, config.effort.as_str());
            spawn(network::record_command(
                ui,
                target,
                CommandId::SessionModel,
                args,
                change_model(ui, target.session_id, config),
            ));
            ui.write().drafts.remove(&target);
        }
        MenuAction::SetEffort(effort) => select_session_effort(ui, target, effort),
        MenuAction::Run { id, arguments } => run_command(ui, target, root, id, arguments, composer),
        // Desktop drafts are created sessions, so their probe is fixed.
        MenuAction::ChooseProbe(_) => {}
    }
}

/// Run a slash command typed to this composer, as the TUI does with the same
/// command; commands that show something open the GUI's own view of it.
fn run_command(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    root: bool,
    id: CommandId,
    arguments: String,
    mut composer: ComposerState,
) {
    // The composer explains unavailable commands instead of running them.
    if !id.is_available(command_context(root)) {
        return;
    }
    match id {
        // Reached only with the menu dismissed: reopen it on the typed text.
        CommandId::SessionModel | CommandId::SessionEffort | CommandId::Agent => {
            let text = format!("/{} {arguments}", id.spec().name);
            ui.write().drafts.insert(target, text);
            composer.dismissed.set(None);
            return;
        }
        // The server records these and the network call clears the draft.
        CommandId::Compact => {
            spawn(network::submit_compact(ui, target, arguments));
            return;
        }
        CommandId::Goal => {
            spawn(network::submit_goal(ui, target, arguments));
            return;
        }
        CommandId::Refresh => {
            refresh_configs(ui, target);
            return;
        }
        _ => {}
    }

    ui.write().drafts.remove(&target);
    match id {
        CommandId::Info => composer.info_open.set(true),
        CommandId::Rename if arguments.is_empty() => {
            composer.prompt.set(Some(SlashPrompt::RenameSession))
        }
        CommandId::Rename => spawn_rename(ui, target, arguments),
        CommandId::Fork => {
            spawn(network::record_command(
                ui,
                target,
                CommandId::Fork,
                arguments.clone(),
                network::fork_session(ui, target, arguments),
            ));
        }
        CommandId::Sandbox => match SandboxAction::parse(&arguments) {
            Some(action) => {
                spawn(network::record_command(
                    ui,
                    target,
                    CommandId::Sandbox,
                    action.as_str().into(),
                    network::change_session_sandbox(ui, target.session_id, action),
                ));
            }
            None => ui.write().error = Some(format!("Usage: {}", id.spec().usage())),
        },
        CommandId::Sessions => ui.write().sidebar.show_mode(SidebarMode::Browser),
        CommandId::NewSession => start_session_naming(ui, target),
        CommandId::Agents => ui.write().sidebar.show_mode(SidebarMode::Details),
        CommandId::Probes => ui.write().sidebar.show_mode(SidebarMode::Probes),
        CommandId::Server => run_server_command(ui, &arguments),
        CommandId::Settings => ui.write().open_settings(crate::app::SETTINGS_FIRST_TAB),
        CommandId::Help => ui.write().open_settings("commands"),
        CommandId::PinnedModels => ui.write().open_settings("models"),
        CommandId::PinnedDecisionModels => ui.write().open_settings("decision-models"),
        CommandId::Quit => {
            let result = try_consume_context::<crate::GuiBootstrap>()
                .ok_or_else(|| "Quitting is unavailable here.".to_owned())
                .and_then(|bootstrap| bootstrap.platform.request_quit());
            if let Err(error) = result {
                ui.write().error = Some(error);
            }
        }
        // TUI-only: desktop drafts are created sessions on a fixed probe.
        CommandId::Probe => {}
        CommandId::SessionModel
        | CommandId::SessionEffort
        | CommandId::Agent
        | CommandId::Compact
        | CommandId::Goal
        | CommandId::Refresh => {}
    }
}

/// Leaving `/model`'s arguments collapses its list back to pinned models, so
/// the next `/model` starts from them again.
fn collapse_model_list(mut all_models: Signal<bool>, text: &str) {
    if *all_models.peek() && argument_command(text) != Some(CommandId::SessionModel) {
        all_models.set(false);
    }
}

/// `/new`: name a session in this session's folder, as its + button does.
fn start_session_naming(mut ui: Signal<Ui>, target: TranscriptTarget) {
    let folder = ui
        .peek()
        .sessions
        .iter()
        .find(|session| session.id == target.session_id)
        .map(|session| crate::creation::CreationTarget {
            probe: session.probe_id,
            directory: session.working_directory.to_string_lossy().into_owned(),
        });
    let mut state = ui.write();
    let Some(folder) = folder else {
        state.error = Some("The session is still loading; try again.".into());
        return;
    };
    crate::creation::select_target(&mut state, folder);
    state.sidebar.show_mode(SidebarMode::Browser);
}

/// `/server status|restart|stop` through the desktop's local server controls.
fn run_server_command(mut ui: Signal<Ui>, arguments: &str) {
    let Some(action) = ServerAction::parse(arguments) else {
        ui.write().error = Some(format!("Usage: {}", CommandId::Server.spec().usage()));
        return;
    };
    let Some(bootstrap) = try_consume_context::<crate::GuiBootstrap>() else {
        return;
    };
    let platform = bootstrap.platform.clone();
    spawn(async move {
        let result = match action {
            ServerAction::Status => platform.local_server_status().await,
            ServerAction::Restart => platform.local_server_restart().await,
            ServerAction::Stop => platform.local_server_stop().await,
        };
        let mut state = ui.write();
        match (action, result) {
            (_, Err(error)) => state.error = Some(format!("Server {}: {error}", action.as_str())),
            (ServerAction::Restart, Ok(LocalServerStatus::Running { connection })) => {
                state.change_connection(connection.api_url, connection.client_key);
                state.notice = Some("Local server restarted.".into());
            }
            (_, Ok(LocalServerStatus::Running { connection })) => {
                state.notice = Some(format!("Local server running at {}", connection.api_url));
            }
            (_, Ok(LocalServerStatus::Stopped)) => {
                state.notice = Some("Local server stopped.".into())
            }
        }
    });
}

fn menu_label(menu: &ComposerMenu, root: bool) -> &'static str {
    if !root {
        return "Slash commands";
    }
    if menu.root_model {
        return "Root agent model policy";
    }
    match menu.menu.command {
        None => "Slash commands",
        Some(CommandId::SessionModel) => "Session default model",
        Some(CommandId::SessionEffort) => "Session default effort",
        Some(CommandId::Agent) => "Agent configurations",
        Some(_) => menu.menu.title,
    }
}

fn render_command_menu(
    menu: &ComposerMenu,
    root: bool,
    selected: Signal<usize>,
    mut popup: Signal<Option<std::rc::Rc<MountedData>>>,
    nodes: MenuNodes,
    onselect: EventHandler<MenuAction>,
) -> Element {
    let label = menu_label(menu, root);
    rsx! {
        div { class:"popup command-popup", role:"listbox", "aria-label":label,
            onmounted:move |event| popup.set(Some(event.data())),
            div { class:"command-heading", "{menu.menu.title}" }
            if let Some(note) = &menu.menu.note { p { class:"muted", "{note}" } }
            if menu.menu.candidates.iter().all(|option| option.kind == CandidateKind::Toggle) {
                p { class:"command-empty", "{menu.menu.empty_hint}" }
            }
            for (index, option) in menu.menu.candidates.iter().enumerate() {
                { let action = option.action.clone(); let toggle = option.kind == CandidateKind::Toggle; rsx! {
                    button {
                        class:match (selected() == index, toggle) {
                            (true, true) => "command-option command-toggle selected",
                            (true, false) => "command-option selected",
                            (false, true) => "command-option command-toggle",
                            (false, false) => "command-option",
                        },
                        role:"option",
                        "aria-selected":if root { AttributeValue::Bool(selected() == index) } else { AttributeValue::Text((selected() == index).to_string()) },
                        onmounted:move |event| register_menu_option(nodes, index, event.data()),
                        onmousedown:move |event| event.prevent_default(),
                        onclick:move |_| onselect.call(action.clone()),
                        span { class:"command-title", "{option.label}" }
                        if !option.detail.is_empty() { span { class:"command-description", "{option.detail}" } }
                    }
                }}
            }
            div { class:"command-hint",
                if menu.menu.candidates.iter().any(|option| option.completion.is_some()) { "↑↓ navigate · Enter select · Tab complete · Esc dismiss" }
                else { "↑↓ navigate · Enter select · Esc dismiss" }
            }
        }
    }
}

#[component]
pub(crate) fn DraftComposer(tab: Tab) -> Element {
    rsx! { ComposerEditor { key:"{tab.target.session_id.0}:{tab.target.agent_id.0}", tab, root:true } }
}

#[component]
pub(crate) fn Composer(tab: Tab) -> Element {
    rsx! { ComposerEditor { key:"{tab.target.session_id.0}:{tab.target.agent_id.0}", tab, root:false } }
}

#[component]
fn ComposerEditor(tab: Tab, root: bool) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let target = tab.target;
    #[cfg(test)]
    composer_tests::record_work(target, 0);
    let session_id = target.session_id;
    let initial_browser_key = use_hook(move || {
        if let Some(key) = ui.peek().drafts.browser_key(&target) {
            return key.to_owned();
        }
        ui.write().drafts.ensure_browser_key(target)
    });
    let composer = ComposerState {
        selected: use_signal(|| 0usize),
        dismissed: use_signal(|| None::<u64>),
        choosing_model: use_signal(|| None::<u64>),
        all_models: use_signal(|| false),
        info_open: use_signal(|| false),
        prompt: use_signal(|| None::<SlashPrompt>),
    };
    let ComposerState {
        mut selected,
        mut dismissed,
        mut choosing_model,
        mut all_models,
        mut info_open,
        mut prompt,
    } = composer;
    let menu_popup = use_signal(|| None::<std::rc::Rc<MountedData>>);
    let menu_nodes = use_signal(Vec::new);
    let mut textarea_node = use_signal(|| None::<std::rc::Rc<MountedData>>);

    use_effect(move || {
        if root {
            spawn(network::load_draft_root(ui, session_id));
        }
    });
    // No global render subscription: other targets' typing, event streams and
    // workspace changes cannot wake this editor.
    use_ui_subscription(move |state| {
        (
            state.drafts.revision(&target),
            state.drafts.mutation(&target),
            state.uncertain_submissions.contains(&target),
            state.transcript_status.get(&target).cloned(),
            state.generation,
            state.settings.preferences.conversation_max_width_ch,
        )
    });
    let session = use_ui_selector(move |state| {
        let (path, probe) = info_location(state, session_id);
        let session = state
            .sessions
            .iter()
            .find(|session| session.id == session_id);
        let current = session.map(|session| session.model_config.clone());
        let name = session.map(|session| session.name.clone());
        (path, probe, current, name)
    });
    let root_draft = use_ui_selector(move |state| {
        if root {
            state.draft_roots.get(&session_id).cloned()
        } else {
            None
        }
    });
    let sources = use_ui_selector(move |state| {
        (
            if root {
                None
            } else {
                state.transcripts.get(&target).cloned()
            },
            state.models.clone(),
        )
    });
    // Catalog changes matter to an open picker, not every hidden editor.
    use_ui_subscription(move |state| {
        let snapshot = state.drafts.snapshot(&target);
        let clock = snapshot.map(|snapshot| snapshot.revision.clock);
        let text = state
            .drafts
            .get(&target)
            .map(String::as_str)
            .unwrap_or_default();
        let open = !dismissed().is_some_and(|value| Some(value) == clock);
        let picking = matches!(
            argument_command(text),
            Some(CommandId::SessionModel | CommandId::SessionEffort)
        );
        (open && picking).then(|| state.models.clone())
    });
    let status = use_memo(move || {
        #[cfg(test)]
        composer_tests::record_work(target, 1);
        let sources = sources.read();
        let (model, effort, tokens, window) = if root {
            let (model, effort) = draft_status_model(
                root_draft
                    .read()
                    .as_ref()
                    .and_then(|draft| draft.model.as_ref()),
                session.read().2.as_ref(),
            );
            let window = model
                .as_ref()
                .and_then(|model| sources.1.iter().find(|info| info.model == *model))
                .and_then(|info| info.context_window);
            (model, effort, None, window)
        } else {
            let projected = sources
                .0
                .as_ref()
                .map(|transcript| agent_status(transcript, &sources.1));
            let config = projected
                .as_ref()
                .and_then(|status| status.model_config.as_ref());
            (
                config.map(|config| config.model.clone()),
                config.map(|config| config.effort),
                projected.as_ref().and_then(|status| status.context_tokens),
                projected.as_ref().and_then(|status| status.context_window),
            )
        };
        let name = model
            .as_ref()
            .map(|model| model_display_name(&sources.1, model));
        (name, effort, tokens, window)
    });
    // Reading the selected source is also needed when loading/pending changes
    // without changing the resolved status.
    let source = root_draft.read();
    let state = ui.peek();
    let text = state.drafts.get(&target).cloned().unwrap_or_default();
    let mutation = state.drafts.mutation(&target);
    let menu = build_command_menu(
        &state,
        target,
        &text,
        root,
        dismissed().is_some_and(|clock| {
            state
                .drafts
                .snapshot(&target)
                .is_some_and(|snapshot| snapshot.revision.clock == clock)
        }),
        choosing_model().is_some_and(|clock| {
            state
                .drafts
                .snapshot(&target)
                .is_some_and(|snapshot| snapshot.revision.clock == clock)
        }),
        all_models(),
    );
    let menu_open = menu.is_some();
    let pending = root
        && source
            .as_ref()
            .is_none_or(|draft| draft.loading || draft.pending);
    let uncertain = state.uncertain_submissions.contains(&target)
        || root && source.as_ref().is_some_and(|draft| draft.uncertain);
    let parsed = parse_composer_input(&text, Frontend::Gui);
    let blocked = pending || uncertain || matches!(parsed, ComposerInput::Command { .. });
    let browser_key = state
        .drafts
        .browser_key(&target)
        .unwrap_or(&initial_browser_key)
        .to_owned();
    let generation = state.generation;
    let width = state.settings.preferences.conversation_max_width_ch;
    let transcript_status = state
        .transcript_status
        .get(&target)
        .filter(|status| *status != "Live")
        .cloned();
    let (status_name, status_effort, status_tokens, status_window) = status();

    let onselect = EventHandler::new(move |action: MenuAction| {
        let keeps_focus = matches!(
            &action,
            MenuAction::Run {
                id: CommandId::Info | CommandId::Rename,
                ..
            }
        );
        apply_menu_choice(ui, target, root, action, composer);
        dismissed.set(None);
        if !keeps_focus {
            focus_node(textarea_node);
        }
    });
    let onkey = EventHandler::new(move |event: InputKeyEvent| {
        let changed = ui
            .peek()
            .drafts
            .snapshot(&target)
            .is_none_or(|previous| previous.text != event.snapshot.text);
        if event.snapshot.composing
            || !ui
                .write()
                .drafts
                .apply_snapshot(target, event.snapshot.clone())
        {
            return;
        }
        // A replacement has not reached this key's DOM yet. Do not submit the
        // optimistic store value on behalf of an older native key.
        if ui.peek().drafts.mutation(&target).is_some()
            && ui
                .peek()
                .drafts
                .get(&target)
                .map(String::as_str)
                .unwrap_or_default()
                != event.snapshot.text
        {
            return;
        }
        if changed {
            selected.set(0);
            dismissed.set(None);
            choosing_model.set(None);
            collapse_model_list(all_models, &event.snapshot.text);
        }
        let menu = build_command_menu(
            &ui.peek(),
            target,
            &event.snapshot.text,
            root,
            *dismissed.peek() == Some(event.snapshot.revision.clock),
            *choosing_model.peek() == Some(event.snapshot.revision.clock),
            *all_models.peek(),
        );
        if let Some(menu) = menu {
            let count = menu.menu.candidates.len();
            match event.key() {
                Key::ArrowDown | Key::ArrowUp => {
                    event.prevent_default();
                    if count > 0 {
                        let delta = if event.key() == Key::ArrowDown {
                            1
                        } else {
                            count - 1
                        };
                        let next = (selected() % count + delta) % count;
                        selected.set(next);
                        scroll_menu_option(menu_popup, menu_nodes, selected, next);
                    }
                }
                Key::Escape => {
                    event.prevent_default();
                    event.stop_propagation();
                    dismissed.set(Some(event.snapshot.revision.clock));
                    choosing_model.set(None);
                    all_models.set(false);
                }
                Key::Tab if !event.modifiers().contains(Modifiers::SHIFT) => {
                    event.prevent_default();
                    let Some(option) = menu.menu.candidates.get(selected() % count.max(1)) else {
                        return;
                    };
                    if option.kind == CandidateKind::Toggle {
                        apply_menu_choice(ui, target, root, option.action.clone(), composer);
                        return;
                    }
                    let Some(text) = option.completion.clone() else {
                        return;
                    };
                    // Keep the completed option selected under the narrowed filter.
                    let narrowed = build_command_menu(
                        &ui.peek(),
                        target,
                        &text,
                        root,
                        false,
                        false,
                        *all_models.peek(),
                    );
                    let index = narrowed
                        .and_then(|menu| {
                            menu.menu
                                .candidates
                                .iter()
                                .position(|o| o.label == option.label)
                        })
                        .unwrap_or(0);
                    ui.write().drafts.insert(target, text);
                    selected.set(index);
                }
                Key::Enter if !event.modifiers().contains(Modifiers::SHIFT) => {
                    event.prevent_default();
                    if let Some(option) = menu.menu.candidates.get(selected()) {
                        apply_menu_choice(ui, target, root, option.action.clone(), composer);
                    }
                }
                _ => {}
            }
            return;
        }
        if event.key() == Key::Escape && !root {
            event.prevent_default();
            event.stop_propagation();
            spawn(network::submit(ui, target, true));
            return;
        }
        if event.key() != Key::Enter || event.modifiers().contains(Modifiers::SHIFT) {
            return;
        }
        event.prevent_default();
        match parse_composer_input(&event.snapshot.text, Frontend::Gui) {
            ComposerInput::Command { id, arguments } if id.is_available(command_context(root)) => {
                run_command(ui, target, root, id, arguments, composer);
            }
            _ if root => {
                spawn_forever(network::submit_draft_root(ui, target.session_id));
            }
            _ => {
                ui.write().workspace.promote(tab.id);
                spawn(network::submit(ui, target, false));
            }
        }
    });
    #[cfg(test)]
    composer_tests::record_key_handler(tab.id, onkey);

    rsx! {
        div { class:"composer", style:"--conversation-max-width:var(--psa-conversation-width,{width}rch)",
            if info_open() {
                if root {
                    crate::agent_info::DraftAgentInfo {
                        name:source.as_ref().and_then(|draft| draft.selected.as_ref()).map(|id| id.name.clone()).unwrap_or_else(|| "Unselected root agent".into()),
                        template:source.as_ref().and_then(|draft| draft.selected.as_ref()).map(|id| id.name.clone()),
                        path:session.read().0.clone(), probe:session.read().1,
                        model:status_name.clone(), effort:status_effort,
                        onclose:move |_| info_open.set(false),
                    }
                } else {
                    crate::agent_info::AgentInfo { target, onclose:move |_| info_open.set(false) }
                }
            }
            if let Some(menu) = &menu {
                {render_command_menu(menu, root, selected, menu_popup, menu_nodes, onselect)}
            }
            if !root && matches!(parsed, ComposerInput::Command { id:CommandId::Agent, .. }) {
                p { class:"command-hint", "The /agent command is only available before creating the root agent. Use //agent to send it as text." }
            }
            if root && matches!(parsed, ComposerInput::Command { id, .. } if !id.is_available(CommandContext::DraftRoot)) {
                p { class:"command-hint", "This command needs an existing agent. Start with // to send it as text." }
            }
            if let Some(kind) = prompt() {
                match kind {
                    SlashPrompt::RenameSession => rsx! {
                        CommandPrompt {
                            label: "Rename session",
                            placeholder: "Session name",
                            initial_value: session.read().3.clone().unwrap_or_default(),
                            submit_label: "Rename",
                            onsubmit: move |name: String| {
                                prompt.set(None);
                                spawn_rename(ui, target, name);
                                focus_node(textarea_node);
                            },
                            oncancel: move |_| {
                                prompt.set(None);
                                focus_node(textarea_node);
                            },
                        }
                    },
                }
            }
            div { class:"composer-input",
                BrowserInput {
                    initial_value:text,
                    mutation,
                    key:browser_key.clone(),
                    draft_key:format!("psa:{generation}:{browser_key}"),
                    multiline:true, autogrow:true, composer:true, root_commands:root, menu_open,
                    aria_label:"Message to agent",
                    disabled:pending,
                    onmounted:move |event: MountedEvent| textarea_node.set(Some(event.data())),
                    onedit:move |snapshot: InputSnapshot| {
                        let edited = snapshot.is_edit();
                        let text = snapshot.text.clone();
                        let accepted = ui.write().drafts.apply_snapshot(target, snapshot);
                        if accepted && edited {
                            ui.write().workspace.promote(tab.id);
                            selected.set(0);
                            dismissed.set(None);
                            choosing_model.set(None);
                            collapse_model_list(all_models, &text);
                        }
                    },
                    onsubmit:move |snapshot:InputSnapshot| {
                        if snapshot.composing || !ui.write().drafts.apply_snapshot(target, snapshot.clone()) {
                            return;
                        }
                        if matches!(parse_composer_input(&snapshot.text, Frontend::Gui), ComposerInput::Command { .. })
                            || ui.peek().drafts.mutation(&target).is_some()
                                && ui.peek().drafts.get(&target).map(String::as_str).unwrap_or_default() != snapshot.text
                        {
                            return;
                        }
                        if root { spawn_forever(network::submit_draft_root(ui, session_id)); }
                        else { ui.write().workspace.promote(tab.id); spawn(network::submit(ui, target, false)); }
                    },
                    onkey,
                }
                button { class:"primary composer-send", "aria-label":if root { "Send first message" } else { "Send steer" },
                    disabled:blocked,
                    onclick:move |_| {
                        if root { spawn_forever(network::submit_draft_root(ui, session_id)); }
                        else { ui.write().workspace.promote(tab.id); spawn(network::submit(ui, target, false)); }
                    },
                    "↑"
                }
            }
            ComposerStatus { path:session.read().0.clone(), model:status_name, effort:status_effort, context_tokens:status_tokens, context_window:status_window }
            if !root && (uncertain || transcript_status.is_some()) {
                div { class:"composer-actions",
                    if uncertain { button { onclick:move |_| { ui.write().uncertain_submissions.remove(&target); }, "I checked delivery; enable retry" } }
                    if let Some(status) = transcript_status { span { class:"muted", "{status}" } }
                }
            }
        }
    }
}

fn question_answer_values(
    event: &dioxus::html::FormData,
    count: usize,
    answers: &DraftStore<usize>,
) -> Vec<String> {
    (0..count)
        .map(|index| match event.get_first(&format!("answer-{index}")) {
            Some(dioxus::html::FormValue::Text(value)) => value,
            _ => answers.get(&index).cloned().unwrap_or_default(),
        })
        .collect()
}

fn submit_question_answers(
    mut ui: Signal<Ui>,
    query: ClientQueryRequest,
    answers: Vec<String>,
    mut submitting: Signal<bool>,
) {
    if *submitting.peek() || answers.iter().any(|answer| answer.trim().is_empty()) {
        return;
    }
    let response = ClientQueryResponse::AskQuestion(AskQuestionResponse {
        answers: answers
            .into_iter()
            .map(|answer| ClientAnswer {
                answer,
                notes: None,
            })
            .collect(),
    });
    let generation = ui.peek().generation;
    submitting.set(true);
    spawn(async move {
        let failed = match network::client(ui) {
            Ok(api) => api
                .respond_to_client_query(query.session_id, query.agent_id, query.id, &response)
                .await
                .is_err(),
            Err(_) => true,
        };
        if ui.peek().generation != generation {
            return;
        }
        if failed {
            ui.write().error =
                Some("Unable to answer question; it may have been answered elsewhere.".into());
        }
        submitting.set(false);
    });
}

#[component]
pub(crate) fn Question(query: ClientQueryRequest) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let ClientQuery::AskQuestion(request) = &query.payload;
    let count = request.questions.len();
    let submit_query = query.clone();
    let mut answers = use_signal(DraftStore::<usize>::default);
    let submitting = use_signal(|| false);
    let form_id = format!(
        "question-{}-{}-{}",
        query.session_id.0, query.agent_id.0, query.id
    );
    rsx! {
        section { class:"question", h4 { "Agent needs your input" }
            form { id:form_id.clone(),
                onsubmit:move |event:FormEvent| {
                    event.prevent_default();
                    // A form event serializes native values together. A queued
                    // per-input edit packet cannot make a pointer submit stale.
                    let values = question_answer_values(&event.data(), count, &answers.peek());
                    if values.iter().enumerate().any(|(index, value)| {
                        answers.peek().mutation(&index).is_some()
                            && answers.peek().get(&index).map(String::as_str).unwrap_or_default() != value
                    }) { return; }
                    submit_question_answers(ui, submit_query.clone(), values, submitting);
                },
                for (index, question) in request.questions.iter().enumerate() {
                    { let form_id = form_id.clone(); rsx! {
                        label { "{question.question}" }
                        for choice in question.choices.clone() {
                            button { r#type:"button", onclick:move |_| { answers.write().insert(index, choice.clone()); }, "{choice}" }
                        }
                        BrowserInput {
                            name:format!("answer-{index}"),
                            required:true,
                            initial_value:answers.read().get(&index).cloned().unwrap_or_default(),
                            mutation:answers.read().mutation(&index),
                            aria_label:question.question.clone(),
                            onedit:move |snapshot:InputSnapshot| { answers.write().apply_snapshot(index, snapshot); },
                            onkey:move |event:InputKeyEvent| {
                                let enter = event.key() == Key::Enter && !event.modifiers().contains(Modifiers::SHIFT);
                                if !answers.write().apply_snapshot(index, event.snapshot) { return; }
                                if enter {
                                    dioxus::document::eval(&format!("document.getElementById({})?.requestSubmit();",
                                        serde_json::to_string(&form_id).expect("form id serializes")));
                                }
                            },
                        }
                    }}
                }
                button { class:"primary", r#type:"submit", disabled:submitting(), "Submit answers" }
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/composer/composer_tests.rs"]
mod composer_tests;
