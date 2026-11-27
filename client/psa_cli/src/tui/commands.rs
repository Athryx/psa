//! Slash commands from the shared registry, as the TUI implements them.
use super::{
    app::{App, AppEvent, Overlay, PromptAction, PromptDialog, SettingsTab},
    composer::Composer,
    picker::{AGENTS_TITLE, Picker, PickerItem, PickerValue, SESSIONS_TITLE},
};
use client_shared::{
    COMMANDS, CommandContext, CommandId, CommandMenu, Frontend, LocalServerStatus, MenuAction,
    MenuContext, SandboxAction, ServerAction, agent_tree, build_command_menu, build_effort_config,
    build_root_model_menu, cache_hit_rate, group_digits, local::LocalServerManager,
    token_difference,
};
use llm_api::providers::{Effort, Model, ModelConfig};
use psa::api::{
    Agent, AgentCommand, AgentState, GoalCommand, ModelInfo, ModelKind, ModelUsage, Probe, Sandbox,
    Session, UpdateSession,
};
use std::sync::Arc;

/// The `/info` panel's title, so per-model usage can fill it in later.
const INFO_TITLE: &str = "Agent";

pub const KEYS: &[(&str, &str)] = &[
    ("Enter", "send, or run the highlighted command"),
    ("Alt+Enter, Ctrl+J", "new line"),
    ("Tab", "complete a command or option"),
    ("Esc", "close a dialog, or interrupt the agent"),
    ("PgUp, PgDn, Ctrl+End", "scroll the transcript"),
    (
        "Ctrl+O",
        "show everything: thinking, harness messages, logs, full tool output",
    ),
    ("Ctrl+Q", "answer the agent's pending question"),
    ("Ctrl+C", "clear the input; twice to quit"),
    ("Tab in /settings", "switch between appearance and commands"),
    ("//text", "send text starting with a slash"),
];

impl App {
    /// Whether `id` can run now.
    pub fn is_command_available(&self, id: CommandId) -> bool {
        id.is_available(self.command_context())
    }

    /// The completion menu for the composer's text, unless it was dismissed.
    pub fn command_menu(&self) -> Option<CommandMenu> {
        if self.completion.dismissed {
            return None;
        }
        let root = self.draft.as_ref().map(|draft| &draft.root);
        if let Some(root) = root.filter(|_| self.choosing_root_model && self.composer.is_empty()) {
            return Some(build_root_model_menu(root));
        }
        let context = MenuContext {
            frontend: Frontend::Tui,
            context: self.command_context(),
            models: &self.models,
            session_model: self.session_model(),
            show_all_models: self.show_all_models,
            local_server: self.local_server.is_some(),
            root,
            probes: &self.probes,
        };
        build_command_menu(self.composer.text(), &context)
    }

    /// Run what a menu choice does.
    pub fn apply_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::Fill(text) => {
                self.composer.set_text(text);
                self.completion.reset();
            }
            MenuAction::ToggleAllModels => self.show_all_models = !self.show_all_models,
            MenuAction::Run { id, arguments } => {
                self.clear_composer();
                self.run_command(id, arguments);
            }
            MenuAction::SetModel(config) if self.draft.is_some() => {
                self.clear_composer();
                self.set_draft_model(config);
            }
            MenuAction::SetModel(config) => {
                self.clear_composer();
                self.set_session_model(config);
            }
            MenuAction::SetEffort(effort) if self.draft.is_some() => {
                self.clear_composer();
                self.set_draft_effort(effort);
            }
            MenuAction::SetEffort(effort) => {
                self.clear_composer();
                self.set_session_effort(effort);
            }
            MenuAction::ChooseConfig(id) => {
                self.clear_composer();
                self.choose_draft_config(id);
            }
            MenuAction::ChooseRootModel(model) => {
                self.clear_composer();
                self.choose_draft_root_model(model);
            }
            MenuAction::ChooseProbe(probe_id) => {
                self.clear_composer();
                self.choose_draft_probe(probe_id);
            }
        }
    }

    pub fn clear_composer(&mut self) {
        self.composer.clear();
        self.completion.reset();
        self.show_all_models = false;
    }

    pub fn run_command(&mut self, id: CommandId, arguments: String) {
        if !self.is_command_available(id) {
            let message = match self.command_context() {
                CommandContext::SessionDraft => {
                    "This needs a session; send a first message to create it."
                }
                _ => "Open a session first: /sessions or /new.",
            };
            self.set_error(message);
            return;
        }
        if self.run_draft_command(id, &arguments) {
            return;
        }
        match id {
            CommandId::Sessions => {
                self.overlay = Some(Overlay::Picker(Picker::loading(SESSIONS_TITLE)));
                self.network.load_sessions();
            }
            CommandId::NewSession => self.start_draft(),
            CommandId::Agents => {
                let view = self.view.as_ref().unwrap();
                let picker = Picker::loading(AGENTS_TITLE).with_header(self.describe_session());
                self.network.load_agents(view.session_id);
                self.overlay = Some(Overlay::Picker(picker));
            }
            CommandId::Info => {
                let lines = self.describe_agent(None);
                self.show_panel(INFO_TITLE, lines);
                let view = self.view.as_ref().unwrap();
                self.network
                    .load_model_usage(view.session_id, view.agent_id);
            }
            CommandId::PinnedModels => self.open_pin_picker(ModelKind::Chat),
            CommandId::PinnedDecisionModels => self.open_pin_picker(ModelKind::Decision),
            CommandId::Probes => self.network.show_probes(),
            CommandId::Server => self.run_server_command(&arguments),
            CommandId::Settings => self.open_settings(SettingsTab::Appearance),
            CommandId::Help => self.open_settings(SettingsTab::Commands),
            CommandId::Quit => self.quit = true,
            CommandId::SessionModel => self.change_model(&arguments),
            CommandId::SessionEffort => self.change_effort(&arguments),
            CommandId::Rename => self.rename_session(arguments),
            CommandId::Fork => {
                let view = self.view.as_ref().unwrap();
                self.network
                    .fork_session(view.session_id, view.agent_id, arguments);
            }
            CommandId::Refresh => {
                let view = self.view.as_ref().unwrap();
                self.network.refresh_configs(view.session_id, view.agent_id);
            }
            CommandId::Sandbox => self.change_sandbox(&arguments),
            CommandId::Compact => {
                let instructions = Some(arguments).filter(|text| !text.is_empty());
                self.run_agent_command(AgentCommand::Compact { instructions });
            }
            CommandId::Goal => {
                self.run_agent_command(AgentCommand::Goal(GoalCommand::parse(&arguments)));
            }
            // Draft commands, handled above.
            CommandId::Agent | CommandId::Probe => {}
        }
    }

    pub fn choose(&mut self, value: PickerValue) {
        if let PickerValue::Pin { model, pinned } = value {
            self.network.set_model_pinned(model, !pinned);
            return;
        }
        self.overlay = None;
        match value {
            PickerValue::Pin { .. } => {}
            PickerValue::Session(id) => self.network.open_session(id),
            PickerValue::Agent(agent_id) => {
                let session_id = self.view.as_ref().unwrap().session_id;
                self.open_agent(session_id, agent_id);
            }
        }
    }

    fn open_settings(&mut self, tab: SettingsTab) {
        self.overlay = Some(Overlay::Settings {
            tab,
            selected: 0,
            changed: false,
        });
    }

    fn show_panel(&mut self, title: &str, lines: Vec<String>) {
        self.overlay = Some(Overlay::Panel {
            title: title.into(),
            lines,
            scroll: 0,
        });
    }

    fn run_agent_command(&mut self, command: AgentCommand) {
        let view = self.view.as_ref().unwrap();
        self.network
            .run_agent_command(view.session_id, view.agent_id, command);
    }

    /// The open session's model, or the one a draft will be created with.
    fn session_model(&self) -> Option<&ModelConfig> {
        if let Some(draft) = &self.draft {
            return draft.model_config.as_ref();
        }
        let session = self.view.as_ref()?.session()?;
        Some(&session.model_config)
    }

    /// Run a prompt dialog's submitted value; empty values are never submitted.
    pub fn submit_prompt(&mut self, action: PromptAction, value: String) {
        match action {
            PromptAction::RenameSession => self.rename_session(value),
        }
    }

    /// `/rename NAME` renames at once; `/rename` asks for the name, starting
    /// from the current one.
    fn rename_session(&mut self, name: String) {
        if self.draft.is_some() {
            self.run_draft_command(CommandId::Rename, &name);
            return;
        }
        if name.is_empty() {
            let current = self.view.as_ref().and_then(|view| view.session());
            let mut input = Composer::default();
            input.set_text(
                current
                    .map(|session| session.name.clone())
                    .unwrap_or_default(),
            );
            self.overlay = Some(Overlay::Prompt(PromptDialog {
                action: PromptAction::RenameSession,
                title: "Rename session",
                input,
            }));
            return;
        }
        let view = self.view.as_ref().unwrap();
        let update = UpdateSession {
            name: Some(name.clone()),
            ..Default::default()
        };
        let notice = format!("Session renamed to {name}.");
        self.network.update_session(
            view.session_id,
            view.agent_id,
            (CommandId::Rename, name),
            update,
            notice,
        );
    }

    /// Reached only when the menu was dismissed: a typed slug sets the model,
    /// anything else reopens the menu with the text as its search.
    fn change_model(&mut self, arguments: &str) {
        let model = Model::parse_slug(arguments).ok();
        let Some(info) = model.and_then(|model| self.models.iter().find(|m| m.model == model))
        else {
            self.composer.set_text(format!("/model {arguments}"));
            return;
        };
        let preferred = self
            .session_model()
            .map(|config| config.effort)
            .unwrap_or(Effort::Medium);
        let config = ModelConfig {
            model: info.model.clone(),
            effort: client_shared::choose_effort(info, preferred),
        };
        self.set_session_model(config);
    }

    fn change_effort(&mut self, arguments: &str) {
        let effort = client_shared::session_efforts(&self.models, self.session_model())
            .into_iter()
            .find(|effort| effort.as_str().eq_ignore_ascii_case(arguments.trim()));
        match effort {
            Some(effort) => self.set_session_effort(effort),
            None => self.composer.set_text(format!("/effort {arguments}")),
        }
    }

    fn set_session_model(&mut self, config: ModelConfig) {
        if self.session_model().is_none() {
            self.set_error("The session is still loading; try again.");
            return;
        }
        let slug = config.model.slug();
        let arguments = format!("{slug} {}", config.effort.as_str());
        self.update_model_config(
            (CommandId::SessionModel, arguments),
            config,
            format!("Model set to {slug}."),
        );
    }

    fn set_session_effort(&mut self, effort: Effort) {
        let Some(config) = build_effort_config(&self.models, self.session_model(), effort) else {
            self.set_error(format!(
                "The session's model does not support {} effort.",
                effort.as_str()
            ));
            return;
        };
        let name = effort.as_str();
        self.update_model_config(
            (CommandId::SessionEffort, name.into()),
            config,
            format!("Effort set to {name}."),
        );
    }

    fn update_model_config(
        &mut self,
        command: (CommandId, String),
        config: ModelConfig,
        notice: String,
    ) {
        let view = self.view.as_ref().unwrap();
        let update = UpdateSession {
            model_config: Some(config),
            ..Default::default()
        };
        self.network
            .update_session(view.session_id, view.agent_id, command, update, notice);
    }

    fn change_sandbox(&mut self, arguments: &str) {
        let Some(action) = SandboxAction::parse(arguments) else {
            self.set_error(format!("Usage: {}", CommandId::Sandbox.spec().usage()));
            return;
        };
        let view = self.view.as_ref().unwrap();
        let (session_id, agent_id) = (view.session_id, view.agent_id);
        let Some(probe_id) = view.session().map(|session| session.probe_id) else {
            self.set_error("The session is still loading; try again.");
            return;
        };
        self.network
            .change_sandbox(session_id, agent_id, probe_id, action);
        self.set_notice(format!("Sandbox {}…", action.as_str()));
    }

    fn run_server_command(&mut self, arguments: &str) {
        let Some(manager) = self.local_server.clone() else {
            self.set_error("Connected to a remote server; /server manages only a local one.");
            return;
        };
        let Some(action) = ServerAction::parse(arguments) else {
            self.set_error(format!("Usage: {}", CommandId::Server.spec().usage()));
            return;
        };
        let events = self.network.events();
        if action != ServerAction::Status {
            self.set_notice(format!("Server {}…", action.as_str()));
        }
        tokio::spawn(async move {
            let event = match run_server_action(&manager, action).await {
                Ok(event) => event,
                Err(error) => AppEvent::Error(format!("Server {}: {error}", action.as_str())),
            };
            let _ = events.send(event);
        });
    }

    fn describe_session(&self) -> Vec<String> {
        let view = self.view.as_ref().unwrap();
        let Some(session) = view.session() else {
            return vec!["Loading…".into()];
        };
        let probe = self
            .probes
            .iter()
            .find(|probe| probe.metadata.id == session.probe_id)
            .map(describe_probe_name)
            .unwrap_or_else(|| session.probe_id.to_string());
        let mut lines = vec![
            format!("Name:        {}", session.name),
            format!("ID:          {}", session.id.0),
            format!(
                "Project:     {}",
                session
                    .project_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none (global)".into())
            ),
            format!("Probe:       {probe}"),
            format!("Directory:   {}", session.working_directory.display()),
            format!(
                "Model:       {} · {}",
                session.model_config.model.slug(),
                session.model_config.effort.as_str()
            ),
        ];
        if let Some(model) = &session.decision_model {
            lines.push(format!("Decisions:   {}", model.slug()));
        }
        if let Some(parent) = session.forked_from {
            lines.push(format!("Forked from: session {}", parent.0));
        }
        lines.push(format!(
            "Busy:        {}",
            if session.busy { "yes" } else { "no" }
        ));
        lines.push(format!("Created:     {}", describe_age(session.created_at)));
        lines.push(format!("Updated:     {}", describe_age(session.updated_at)));
        lines.push(format!("Agents:      {}", view.agents.len()));
        lines
    }

    /// `/info`'s lines: the agent, its environment, and its token usage with
    /// the same breakdown as the desktop client. Per-model usage arrives later.
    pub fn describe_agent(&self, model_usage: Option<&[ModelUsage]>) -> Vec<String> {
        let view = self.view.as_ref().unwrap();
        let Some(agent) = view.agent() else {
            return vec!["Loading…".into()];
        };
        let status = self.status();
        let mut lines = vec![
            format!("Name:        {}", agent.name),
            format!("ID:          {}", agent.id.0),
            format!("State:       {}", describe_state(agent.state)),
        ];
        let mut kinds = vec![if agent.interactive {
            "interactive"
        } else {
            "noninteractive"
        }];
        if agent.hidden {
            kinds.push("internal");
        }
        if agent.forked_from.is_some() {
            kinds.push("forked");
        }
        lines.push(format!("Kind:        {}", kinds.join(", ")));
        if let Some(parent) = agent.subagent_parent_id {
            let name = view
                .agents
                .iter()
                .find(|other| other.id == parent)
                .map(|other| other.name.clone())
                .unwrap_or_else(|| format!("agent {}", parent.0));
            lines.push(format!("Parent:      {name}"));
        }
        lines.push(format!("Snapshot:    #{}", agent.definition_id.0));
        if let Some(session) = view.session() {
            lines.push(format!(
                "Directory:   {}",
                session.working_directory.display()
            ));
        }
        if let Some(config) = &status.model_config {
            lines.push(format!(
                "Model:       {} · {}",
                config.model.slug(),
                config.effort.as_str()
            ));
        }
        if let Some(context) = describe_context(&status) {
            lines.push(format!("Context:     {context}"));
        }

        let usage = &agent.usage;
        let cacheable = agent.cacheable_input_tokens;
        let count =
            |value: Option<u64>| value.map(group_digits).unwrap_or_else(|| "unknown".into());
        let hit = cache_hit_rate(usage, cacheable)
            .map(|rate| format!("{:.1}%", rate * 100.0))
            .unwrap_or_else(|| "unknown".into());
        lines.push(String::new());
        lines.push("Tokens".into());
        lines.push(format!(
            "  Input           {:>14}",
            count(usage.input_tokens)
        ));
        lines.push(format!(
            "    Cached        {:>14}",
            count(usage.cached_input_tokens)
        ));
        lines.push(format!("    Cacheable     {:>14}", count(cacheable)));
        lines.push(format!("    Cache hit rate{hit:>14}"));
        lines.push(format!(
            "    Uncached      {:>14}",
            count(token_difference(
                usage.input_tokens,
                usage.cached_input_tokens
            ))
        ));
        if let Some(write) = usage.cache_write_tokens {
            lines.push(format!("    Cache write   {:>14}", group_digits(write)));
        }
        lines.push(format!(
            "  Output          {:>14}",
            count(usage.output_tokens)
        ));
        lines.push(format!(
            "    Thinking      {:>14}",
            count(usage.reasoning_output_tokens)
        ));
        lines.push(format!(
            "    Non-thinking  {:>14}",
            count(token_difference(
                usage.output_tokens,
                usage.reasoning_output_tokens
            ))
        ));
        lines.push(format!(
            "  Total           {:>14}",
            count(usage.total_tokens)
        ));
        if let Some(cost) = usage.cost {
            lines.push(format!("  Cost            {cost:>14} credits"));
        }

        // Like the desktop client, only a mix of models gets its own table.
        if let Some(usage) = model_usage.filter(|usage| usage.len() > 1) {
            let short = |value: Option<u64>| describe_count(value);
            lines.push(String::new());
            lines.push(format!(
                "{:<30} {:>9} {:>9} {:>9}",
                "Usage by model", "Input", "Output", "Total"
            ));
            for entry in usage {
                let name = self
                    .models
                    .iter()
                    .find(|info| info.model == entry.model)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| entry.model.slug());
                lines.push(format!(
                    "  {:<28} {:>9} {:>9} {:>9}",
                    name,
                    short(entry.usage.input_tokens),
                    short(entry.usage.output_tokens),
                    short(entry.usage.total_tokens)
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!(
            "Created {} · updated {}",
            describe_age(agent.created_at),
            describe_age(agent.updated_at)
        ));
        lines
    }

    pub fn is_info_open(&self) -> bool {
        matches!(&self.overlay, Some(Overlay::Panel { title, .. }) if title == INFO_TITLE)
    }

    /// `/pinned-models` and `/pinned-decision-models`: every model of `kind`,
    /// where Enter pins or unpins the highlighted one.
    fn open_pin_picker(&mut self, kind: ModelKind) {
        let title = pin_picker_title(kind);
        let picker = Picker::new(title, pin_items(&self.models, kind))
            .with_header(vec![
                "Pins are stored on the server and shared with its other clients.".into(),
            ])
            .with_enter_hint("pin or unpin");
        self.overlay = Some(Overlay::Picker(picker));
    }

    /// Show a changed pin in an open pin picker, keeping its place.
    pub fn refresh_pin_picker(&mut self) {
        let Some(Overlay::Picker(picker)) = &mut self.overlay else {
            return;
        };
        for kind in [ModelKind::Chat, ModelKind::Decision] {
            if picker.title == pin_picker_title(kind) {
                picker.update_items(pin_items(&self.models, kind));
            }
        }
    }
}

async fn run_server_action(
    manager: &Arc<LocalServerManager>,
    action: ServerAction,
) -> std::io::Result<AppEvent> {
    match action {
        ServerAction::Status => {
            let status = manager.status().await?;
            let state = match &status {
                LocalServerStatus::Running { connection } => {
                    format!("Running at {}", connection.api_url)
                }
                LocalServerStatus::Stopped => "Stopped".into(),
            };
            Ok(AppEvent::Panel {
                title: "Local server".into(),
                lines: vec![
                    state,
                    format!("Data dir: {}", manager.data_dir().display()),
                    format!("Log:      {}", manager.log_path().display()),
                ],
            })
        }
        ServerAction::Restart => match manager.restart().await? {
            LocalServerStatus::Running { connection } => Ok(AppEvent::ServerRestarted(connection)),
            LocalServerStatus::Stopped => Ok(AppEvent::Error("The server did not start.".into())),
        },
        ServerAction::Stop => {
            manager.stop().await?;
            Ok(AppEvent::ServerStopped)
        }
    }
}

pub fn session_items(sessions: &[Session], probes: &[Probe]) -> Vec<PickerItem> {
    sessions
        .iter()
        .map(|session| {
            let probe = probes
                .iter()
                .find(|probe| probe.metadata.id == session.probe_id)
                .map(describe_probe_name)
                .unwrap_or_else(|| "unknown probe".into());
            let busy = if session.busy { " · busy" } else { "" };
            PickerItem {
                label: session.name.clone(),
                detail: format!(
                    "{}{busy} · {probe} · {}",
                    describe_age(session.updated_at),
                    session.working_directory.display()
                ),
                value: PickerValue::Session(session.id),
            }
        })
        .collect()
}

fn pin_picker_title(kind: ModelKind) -> &'static str {
    match kind {
        ModelKind::Chat => "Pinned models",
        ModelKind::Decision => "Pinned decision models",
    }
}

/// Models of `kind` in catalog order, so a pin change never moves the
/// highlighted row; pinned ones are starred.
pub fn pin_items(models: &[ModelInfo], kind: ModelKind) -> Vec<PickerItem> {
    models
        .iter()
        .filter(|model| model.kind == kind)
        .map(|model| PickerItem {
            label: format!("{} {}", if model.pinned { "★" } else { "☆" }, model.name),
            detail: model.model.slug(),
            value: PickerValue::Pin {
                model: model.model.clone(),
                pinned: model.pinned,
            },
        })
        .collect()
}

pub fn agent_items(agents: &[Agent], show_hidden: bool) -> Vec<PickerItem> {
    agent_tree(agents, show_hidden)
        .into_iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let hidden = if row.agent.hidden { " · hidden" } else { "" };
            PickerItem {
                label: format!("{indent}{}", row.agent.name),
                detail: format!("{}{hidden}", describe_state(row.agent.state)),
                value: PickerValue::Agent(row.agent.id),
            }
        })
        .collect()
}

pub fn describe_probes(probes: &[Probe], sandboxes: &[Sandbox]) -> Vec<String> {
    if probes.is_empty() {
        return vec!["No probes have connected.".into()];
    }
    let mut lines = Vec::new();
    for probe in probes {
        let metadata = &probe.metadata;
        let mut flags = vec![if probe.is_active {
            "connected"
        } else {
            "offline"
        }];
        if metadata.host {
            flags.push("host");
        }
        if metadata.sandboxes {
            flags.push("sandboxes");
        }
        lines.push(format!(
            "{} · {} · {}/{}",
            describe_probe_name(probe),
            flags.join(", "),
            metadata.os,
            metadata.architecture
        ));
        for sandbox in sandboxes.iter().filter(|s| s.manager_id == metadata.id) {
            lines.push(format!(
                "    sandbox {} · {:?} · {}",
                sandbox.name, sandbox.state, sandbox.image
            ));
        }
    }
    lines
}

/// The commands tab of the settings dialog, which `/help` opens.
pub fn describe_help() -> Vec<String> {
    let mut lines = vec!["Commands".to_owned()];
    for command in COMMANDS.iter().filter(|c| c.supports(Frontend::Tui)) {
        lines.push(format!("  {}", command.usage()));
        lines.push(format!("      {}", command.description));
    }
    lines.push(String::new());
    lines.push("Keys".into());
    for (key, description) in KEYS {
        lines.push(format!("  {key:<22} {description}"));
    }
    lines
}

pub fn describe_probe_name(probe: &Probe) -> String {
    match &probe.metadata.label {
        Some(label) => format!("{label} ({})", probe.metadata.hostname),
        None => probe.metadata.hostname.clone(),
    }
}

pub fn describe_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Interrupted => "interrupted",
        AgentState::Running => "running",
    }
}

pub fn describe_context(status: &client_shared::AgentStatus) -> Option<String> {
    let tokens = status.context_tokens?;
    Some(match status.context_window {
        Some(window) if window > 0 => format!(
            "{} / {} tokens ({}%)",
            describe_count(Some(tokens)),
            describe_count(Some(window)),
            tokens * 100 / window
        ),
        _ => format!("{} tokens", describe_count(Some(tokens))),
    })
}

/// A token count such as `12.3k`, or `?` when unreported.
pub fn describe_count(count: Option<u64>) -> String {
    match count {
        None => "?".into(),
        Some(count) if count >= 1_000_000 => format!("{:.1}M", count as f64 / 1_000_000.0),
        Some(count) if count >= 1_000 => format!("{:.1}k", count as f64 / 1_000.0),
        Some(count) => count.to_string(),
    }
}

/// How long ago `time` was, such as `5m ago`.
pub fn describe_age(time: time::OffsetDateTime) -> String {
    let seconds = (time::OffsetDateTime::now_utc() - time)
        .whole_seconds()
        .max(0);
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

#[cfg(test)]
#[path = "../tests/tui/commands/tests.rs"]
mod tests;
