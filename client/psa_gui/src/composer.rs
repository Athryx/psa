//! Target-scoped composer UI and browser-owned editing.
use crate::{
    app::Ui,
    command_prompt::CommandPrompt,
    input::{BrowserInput, DraftStore, InputKeyEvent, InputSnapshot},
    network,
    reactive::{use_ui_selector, use_ui_subscription},
    views::ProbeName,
};
use client_shared::*;
use dioxus::{
    core::{AttributeValue, spawn_forever},
    prelude::*,
};
use llm_api::{
    api::Usage,
    providers::{Effort, Model, ModelConfig},
};
use psa::api::*;

#[cfg(test)]
use crate::transcript_view::TranscriptView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftCommandStage {
    Completion,
    Configurations,
    Models,
    SessionModels,
    SessionEfforts,
    SandboxActions,
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
    if text
        .strip_prefix("/sandbox")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        return Some(DraftCommandStage::SandboxActions);
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
        "refresh",
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
                if let Ok(snapshot) = client.get_resolved_agent_config(id).await {
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
    usage: Option<Usage>,
    mut open: Signal<bool>,
) -> Element {
    crate::app::register_modal(open);
    let count = |value: Option<u64>| value.map(group_digits).unwrap_or_else(|| "Unknown".into());
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
                crate::modal::ModalHeader { title:"Agent information", close_label:"Close agent information", onclose:move |_| open.set(false) }
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
                    if let Some(cache_write) = cache_write { dt { class:"info-detail", "Cache write" } dd { "{group_digits(cache_write)}" } }
                    dt { "Output tokens" } dd { "{count(output)}" }
                    dt { class:"info-detail", "Thinking" } dd { "{count(thinking)}" }
                    dt { class:"info-detail", "Non-thinking" } dd { "{count(token_difference(output, thinking))}" }
                    dt { "Total tokens" } dd { "{count(usage.as_ref().and_then(|u| u.total_tokens))}" }
                }
            }
        }
    }
}

/// Formats a count with comma thousands separators, such as `1,234,567`.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
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
        let state = ui.peek();
        let current = state
            .sessions
            .iter()
            .find(|session| session.id == target.session_id)
            .map(|session| &session.model_config);
        effort_model_config(&state.models, current, effort)
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
            "effort",
            effort.as_str().into(),
            change,
        ));
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
/// `/rename`, recorded in the transcript of the agent it was typed to.
fn spawn_rename(ui: Signal<Ui>, target: TranscriptTarget, name: String) {
    spawn(network::record_command(
        ui,
        target,
        "rename",
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

fn open_slash_prompt(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    mut prompt: Signal<Option<SlashPrompt>>,
    kind: SlashPrompt,
) {
    ui.write().drafts.remove(&target);
    prompt.set(Some(kind));
}

/// `/info` only opens a local view, so it is recorded as finished at once.
fn record_info(ui: Signal<Ui>, target: TranscriptTarget) {
    spawn(network::record_command(
        ui,
        target,
        "info",
        String::new(),
        async { Some(Ok(())) },
    ));
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

#[derive(Clone)]
enum MenuAction {
    Command(CommandSpec),
    Configuration(ConfigId),
    RootModel(ModelSelection),
    SessionModel(ModelConfig),
    SessionEffort(Effort),
    Sandbox(SandboxAction),
}

struct MenuOption {
    title: String,
    description: String,
    action: MenuAction,
    /// Composer text Tab fills in for this option, if any.
    completion: Option<String>,
}

struct CommandMenu {
    stage: DraftCommandStage,
    options: Vec<MenuOption>,
    current: Option<ModelConfig>,
    filtered: bool,
}

fn command_stage(
    text: &str,
    root: bool,
    dismissed: bool,
    choosing_model: bool,
) -> Option<DraftCommandStage> {
    let stage = draft_command_stage(text, dismissed, choosing_model && root)?;
    if !root && stage == DraftCommandStage::Configurations {
        return None;
    }
    if stage == DraftCommandStage::Completion
        && !complete_commands(text)
            .iter()
            .any(|command| command.id.is_available(root))
    {
        return None;
    }
    Some(stage)
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
) -> Option<CommandMenu> {
    let stage = command_stage(text, root, dismissed, choosing_model)?;
    #[cfg(test)]
    if stage == DraftCommandStage::SessionModels {
        composer_tests::record_work(target, 2);
    }
    let current = state
        .sessions
        .iter()
        .find(|session| session.id == target.session_id)
        .map(|session| session.model_config.clone());
    let mut options = Vec::new();
    let mut filtered = false;

    match stage {
        DraftCommandStage::Completion => {
            options = complete_commands(text)
                .into_iter()
                .filter(|command| command.id.is_available(root))
                .map(|command| MenuOption {
                    title: format!("/{}", command.name),
                    description: command.description.into(),
                    action: MenuAction::Command(command),
                    completion: None,
                })
                .collect();
        }
        DraftCommandStage::Configurations => {
            let filter = text
                .strip_prefix("/agent")
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if let Some(draft) = state.draft_roots.get(&target.session_id) {
                options = draft
                    .configs
                    .iter()
                    .filter(|config| {
                        config
                            .config
                            .config_id
                            .name
                            .to_lowercase()
                            .contains(&filter)
                    })
                    .map(|status| {
                        let config = &status.config;
                        let mut description = format!("{:?}", config.config_id.config_group);
                        if !config.description.is_empty() {
                            description.push_str(" · ");
                            description.push_str(&config.description);
                        }
                        MenuOption {
                            title: config.config_id.name.clone(),
                            description,
                            action: MenuAction::Configuration(config.config_id.clone()),
                            completion: None,
                        }
                    })
                    .collect();
            }
        }
        DraftCommandStage::Models => {
            if let Some(draft) = state.draft_roots.get(&target.session_id) {
                options = draft
                    .model_choices()
                    .into_iter()
                    .map(|selection| MenuOption {
                        title: draft_model_label(&selection),
                        description: String::new(),
                        action: MenuAction::RootModel(selection),
                        completion: None,
                    })
                    .collect();
            }
        }
        DraftCommandStage::SessionModels => {
            let filter = text
                .strip_prefix("/model")
                .unwrap_or("")
                .trim()
                .to_lowercase();
            filtered = !filter.is_empty();
            let preferred = current
                .as_ref()
                .map(|config| config.effort)
                .unwrap_or(Effort::Medium);
            options = state
                .models
                .iter()
                .filter(|model| {
                    (all_models
                        || model.pinned
                        || current
                            .as_ref()
                            .is_some_and(|config| config.model == model.model))
                        && (filter.is_empty()
                            || model.name.to_lowercase().contains(&filter)
                            || model.model.api_name.to_lowercase().contains(&filter))
                })
                .map(|model| MenuOption {
                    title: model.name.clone(),
                    description: format!(
                        "{}/{}",
                        model.model.provider.as_str(),
                        model.model.api_name
                    ),
                    action: MenuAction::SessionModel(ModelConfig {
                        model: model.model.clone(),
                        effort: choose_effort(model, preferred),
                    }),
                    completion: None,
                })
                .collect();
        }
        DraftCommandStage::SandboxActions => {
            let filter = text
                .strip_prefix("/sandbox")
                .unwrap_or("")
                .trim()
                .to_lowercase();
            options = SandboxAction::ALL
                .into_iter()
                .filter(|action| action.as_str().starts_with(&filter))
                .map(|action| MenuOption {
                    title: action.as_str().into(),
                    description: action.description().into(),
                    action: MenuAction::Sandbox(action),
                    completion: None,
                })
                .collect();
        }
        DraftCommandStage::SessionEfforts => {
            let filter = text
                .strip_prefix("/effort")
                .unwrap_or("")
                .trim()
                .to_lowercase();
            options = session_efforts(&state.models, current.as_ref(), &filter)
                .into_iter()
                .map(|effort| MenuOption {
                    title: effort.as_str().into(),
                    description: String::new(),
                    action: MenuAction::SessionEffort(effort),
                    completion: None,
                })
                .collect();
        }
    }
    for option in &mut options {
        option.completion = menu_completion(stage, option);
    }
    Some(CommandMenu {
        stage,
        options,
        current,
        filtered,
    })
}

/// The slash command a picker stage chooses arguments for.
fn picker_command(stage: DraftCommandStage) -> Option<CommandId> {
    match stage {
        DraftCommandStage::Configurations => Some(CommandId::Agent),
        DraftCommandStage::SessionModels => Some(CommandId::SessionModel),
        DraftCommandStage::SessionEfforts => Some(CommandId::SessionEffort),
        DraftCommandStage::SandboxActions => Some(CommandId::Sandbox),
        // The root model policy follows `/agent` and has no typed form.
        DraftCommandStage::Completion | DraftCommandStage::Models => None,
    }
}

/// Tab completes a command name, or a picker option as `/command option`,
/// which the picker's filter then narrows back to that option.
fn menu_completion(stage: DraftCommandStage, option: &MenuOption) -> Option<String> {
    if let MenuAction::Command(command) = &option.action {
        // `/refresh` takes no arguments, so it completes without a trailing space.
        return Some(if command.id == CommandId::Refresh {
            format!("/{}", command.name)
        } else {
            format!("/{} ", command.name)
        });
    }
    let id = picker_command(stage)?;
    let command = COMMANDS.iter().find(|command| command.id == id)?;
    Some(format!("/{} {}", command.name, option.title))
}

fn select_menu_action(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    action: MenuAction,
    mut choosing_model: Signal<Option<u64>>,
    mut info_open: Signal<bool>,
    prompt: Signal<Option<SlashPrompt>>,
) {
    match action {
        MenuAction::Command(command) => match command.id {
            CommandId::Refresh => refresh_configs(ui, target),
            CommandId::Info => {
                ui.write().drafts.remove(&target);
                info_open.set(true);
                record_info(ui, target);
            }
            // Choosing from the menu compacts immediately; Tab completes
            // `/compact ` so focus instructions can be typed first.
            CommandId::Compact => {
                spawn(network::submit_compact(ui, target, String::new()));
            }
            CommandId::Rename => open_slash_prompt(ui, target, prompt, SlashPrompt::RenameSession),
            CommandId::Fork => {
                ui.write().drafts.remove(&target);
                spawn(network::record_command(
                    ui,
                    target,
                    "fork",
                    String::new(),
                    network::fork_session(ui, target, String::new()),
                ));
            }
            _ => {
                ui.write()
                    .drafts
                    .insert(target, format!("/{} ", command.name));
            }
        },
        MenuAction::Configuration(id) => {
            let choose = select_draft_config(ui, target.session_id, target, id);
            let clock = ui
                .peek()
                .drafts
                .snapshot(&target)
                .map(|snapshot| snapshot.revision.clock);
            choosing_model.set(if choose { clock } else { None });
        }
        MenuAction::RootModel(model) => {
            if let Some(draft) = ui.write().draft_roots.get_mut(&target.session_id) {
                draft.model = Some(model);
            }
            choosing_model.set(None);
        }
        MenuAction::SessionModel(config) => {
            let args = format!("{} {}", config.model, config.effort.as_str());
            spawn(network::record_command(
                ui,
                target,
                "model",
                args,
                change_model(ui, target.session_id, config),
            ));
            ui.write().drafts.remove(&target);
        }
        MenuAction::SessionEffort(effort) => select_session_effort(ui, target, effort),
        MenuAction::Sandbox(action) => {
            ui.write().drafts.remove(&target);
            spawn(network::record_command(
                ui,
                target,
                "sandbox",
                action.as_str().into(),
                network::change_session_sandbox(ui, target.session_id, action),
            ));
        }
    }
}

fn render_command_menu(
    menu: &CommandMenu,
    root: bool,
    mut selected: Signal<usize>,
    mut all_models: Signal<bool>,
    mut popup: Signal<Option<std::rc::Rc<MountedData>>>,
    nodes: MenuNodes,
    onselect: EventHandler<MenuAction>,
) -> Element {
    let stage = menu.stage;
    let label = if !root {
        "Slash commands"
    } else {
        match stage {
            DraftCommandStage::Completion => "Slash commands",
            DraftCommandStage::Configurations => "Agent configurations",
            DraftCommandStage::Models => "Root agent model policy",
            DraftCommandStage::SessionModels => "Session default model",
            DraftCommandStage::SessionEfforts => "Session default effort",
            DraftCommandStage::SandboxActions => "Session sandbox",
        }
    };
    let heading = match stage {
        DraftCommandStage::Completion => "Commands",
        DraftCommandStage::Configurations => "Choose a root agent",
        DraftCommandStage::Models => "Choose the root agent model policy",
        DraftCommandStage::SessionModels => "Session default model (not individual agents)",
        DraftCommandStage::SessionEfforts => "Session default effort (not individual agents)",
        DraftCommandStage::SandboxActions => "Session sandbox",
    };
    rsx! {
        div { class:"popup command-popup", role:"listbox", "aria-label":label,
            onmounted:move |event| popup.set(Some(event.data())),
            div { class:"command-heading", "{heading}" }
            if stage == DraftCommandStage::SessionModels {
                if let Some(config) = &menu.current {
                    p { class:"muted", "Current: {config.model.provider.as_str()}/{config.model.api_name} · {config.effort.as_str()}" }
                }
                button { onclick:move |_| { all_models.set(!all_models()); selected.set(0); },
                    if all_models() { "Show favorites" } else { "All models…" }
                }
            }
            if stage == DraftCommandStage::SessionEfforts {
                if let Some(config) = &menu.current { p { class:"muted", "Current: {config.effort.as_str()}" } }
            }
            if menu.options.is_empty() {
                p { class:"command-empty",
                    match stage {
                        DraftCommandStage::Configurations => "No matching usable configurations.",
                        DraftCommandStage::SessionModels if menu.filtered => "No matching models. Try another search or choose All models.",
                        DraftCommandStage::SessionModels => "No favorite models. Open settings to pin models, or choose All models.",
                        DraftCommandStage::SessionEfforts => "No matching effort options for the current model.",
                        _ => "No matching options.",
                    }
                }
            }
            for (index, option) in menu.options.iter().enumerate() {
                { let action = option.action.clone(); rsx! {
                    button {
                        class:if selected() == index { "command-option selected" } else { "command-option" },
                        role:"option",
                        "aria-selected":if root { AttributeValue::Bool(selected() == index) } else { AttributeValue::Text((selected() == index).to_string()) },
                        onmounted:move |event| register_menu_option(nodes, index, event.data()),
                        onmousedown:move |event| event.prevent_default(),
                        onclick:move |_| onselect.call(action.clone()),
                        span { class:"command-title", "{option.title}" }
                        if !option.description.is_empty() { span { class:"command-description", "{option.description}" } }
                    }
                }}
            }
            div { class:"command-hint",
                if menu.options.iter().any(|option| option.completion.is_some()) { "↑↓ navigate · Enter select · Tab complete · Esc dismiss" }
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
    let mut selected = use_signal(|| 0usize);
    // Menu state expires on a shared browser edit, not intentional replacements.
    let mut dismissed = use_signal(|| None::<u64>);
    let mut choosing_model = use_signal(|| None::<u64>);
    let all_models = use_signal(|| false);
    let mut info_open = use_signal(|| false);
    let mut prompt = use_signal(|| None::<SlashPrompt>);
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
        let stage = command_stage(
            state
                .drafts
                .get(&target)
                .map(String::as_str)
                .unwrap_or_default(),
            root,
            dismissed().is_some_and(|value| Some(value) == clock),
            choosing_model().is_some_and(|value| Some(value) == clock),
        );
        matches!(
            stage,
            Some(DraftCommandStage::SessionModels | DraftCommandStage::SessionEfforts)
        )
        .then(|| state.models.clone())
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
    let info = use_ui_selector(move |state| {
        if !info_open() || root {
            return None;
        }
        state
            .agents
            .get(&session_id)
            .and_then(|agents| agents.iter().find(|agent| agent.id == target.agent_id))
            .map(|agent| (agent.name.clone(), agent.definition_id, agent.usage.clone()))
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
    let parsed = parse_composer_input(&text);
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
            MenuAction::Command(command) if matches!(command.id, CommandId::Info | CommandId::Rename)
        );
        select_menu_action(ui, target, action, choosing_model, info_open, prompt);
        selected.set(0);
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
            let count = menu.options.len();
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
                }
                Key::Tab if !event.modifiers().contains(Modifiers::SHIFT) => {
                    event.prevent_default();
                    let completion =
                        menu.options
                            .get(selected() % count.max(1))
                            .and_then(|option| {
                                Some((option.title.clone(), option.completion.clone()?))
                            });
                    if let Some((title, text)) = completion {
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
                                menu.options.iter().position(|option| option.title == title)
                            })
                            .unwrap_or(0);
                        ui.write().drafts.insert(target, text);
                        selected.set(index);
                    }
                }
                Key::Enter if !event.modifiers().contains(Modifiers::SHIFT) => {
                    event.prevent_default();
                    if let Some(option) = menu.options.get(selected()) {
                        select_menu_action(
                            ui,
                            target,
                            option.action.clone(),
                            choosing_model,
                            info_open,
                            prompt,
                        );
                        selected.set(0);
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
        match parse_composer_input(&event.snapshot.text) {
            ComposerInput::Command {
                id: CommandId::Refresh,
                ..
            } => refresh_configs(ui, target),
            ComposerInput::Command {
                id: CommandId::Info,
                ..
            } => {
                ui.write().drafts.remove(&target);
                info_open.set(true);
                record_info(ui, target);
            }
            ComposerInput::Command {
                id: CommandId::Rename,
                arguments,
            } => {
                if arguments.is_empty() {
                    open_slash_prompt(ui, target, prompt, SlashPrompt::RenameSession);
                } else {
                    ui.write().drafts.remove(&target);
                    spawn_rename(ui, target, arguments);
                }
            }
            ComposerInput::Command {
                id: CommandId::Sandbox,
                arguments,
            } => match SandboxAction::parse(&arguments) {
                Some(action) => {
                    ui.write().drafts.remove(&target);
                    spawn(network::record_command(
                        ui,
                        target,
                        "sandbox",
                        action.as_str().into(),
                        network::change_session_sandbox(ui, target.session_id, action),
                    ));
                }
                None => {
                    ui.write().drafts.insert(target, "/sandbox ".into());
                }
            },
            ComposerInput::Command {
                id: CommandId::Fork,
                arguments,
            } if !root => {
                ui.write().drafts.remove(&target);
                spawn(network::record_command(
                    ui,
                    target,
                    "fork",
                    arguments.clone(),
                    network::fork_session(ui, target, arguments),
                ));
            }
            ComposerInput::Command {
                id: CommandId::Compact,
                arguments,
            } if !root => {
                spawn(network::submit_compact(ui, target, arguments));
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
                    AgentInfo {
                        name:source.as_ref().and_then(|draft| draft.selected.as_ref()).map(|id| id.name.clone()).unwrap_or_else(|| "Unselected root agent".into()),
                        template:source.as_ref().and_then(|draft| draft.selected.as_ref()).map(|id| id.name.clone()),
                        definition_id:None, path:session.read().0.clone(), probe:session.read().1,
                        model:status_name.clone(), effort:status_effort, usage:None, open:info_open,
                    }
                } else {
                    AgentInfo {
                        name:info.read().as_ref().map(|info| info.0.clone()).unwrap_or_else(|| "Unknown agent".into()),
                        template:None, definition_id:info.read().as_ref().map(|info| info.1),
                        path:session.read().0.clone(), probe:session.read().1,
                        model:status_name.clone(), effort:status_effort,
                        usage:info.read().as_ref().map(|info| info.2.clone()), open:info_open,
                    }
                }
            }
            if let Some(menu) = &menu {
                {render_command_menu(menu, root, selected, all_models, menu_popup, menu_nodes, onselect)}
            }
            if !root && matches!(parsed, ComposerInput::Command { id:CommandId::Agent, .. }) {
                p { class:"command-hint", "The /agent command is only available before creating the root agent. Use //agent to send it as text." }
            }
            if root && matches!(parsed, ComposerInput::Command { id:CommandId::Compact | CommandId::Fork, .. }) {
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
                        let accepted = ui.write().drafts.apply_snapshot(target, snapshot);
                        if accepted && edited {
                            ui.write().workspace.promote(tab.id);
                            selected.set(0);
                            dismissed.set(None);
                            choosing_model.set(None);
                        }
                    },
                    onsubmit:move |snapshot:InputSnapshot| {
                        if snapshot.composing || !ui.write().drafts.apply_snapshot(target, snapshot.clone()) {
                            return;
                        }
                        if matches!(parse_composer_input(&snapshot.text), ComposerInput::Command { .. })
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
#[path = "composer_tests.rs"]
mod composer_tests;
