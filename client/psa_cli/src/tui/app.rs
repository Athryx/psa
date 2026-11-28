//! TUI state, owned by the app task. Background tasks report through
//! [`AppEvent`]; key handling lives in `input` and slash commands in `commands`.
use super::{
    composer::Composer,
    network::Network,
    picker::{AGENTS_TITLE, Picker, SESSIONS_TITLE},
    settings::{TuiPreferences, TuiSettings},
    theme::Palette,
};
use client_shared::{
    CommandContext, CommandSelection, ConnectionSettings, LocalServerManager, Transcript,
    agent_status,
};
use psa::api::{
    Agent, AgentContextState, AgentId, AgentState, ClientQueryId, ClientQueryRequest, EventsPage,
    ModelInfo, ModelUsage, Probe, Sandbox, Session, SessionId, StreamMessage,
};
use ratatui::layout::Rect;
use std::{path::PathBuf, sync::Arc};
use tokio::task::JoinHandle;

pub enum AppEvent {
    Sessions(Vec<Session>),
    Probes(Vec<Probe>),
    Models(Vec<ModelInfo>),
    ProbeReport {
        probes: Vec<Probe>,
        sandboxes: Vec<Sandbox>,
    },
    Agents {
        session_id: SessionId,
        agents: Vec<Agent>,
    },
    /// Switch the view to this agent.
    OpenAgent {
        session_id: SessionId,
        agent_id: AgentId,
    },
    /// The followed agent's history loaded (again, after a reconnect).
    Opened {
        generation: u64,
        session: Session,
        agents: Vec<Agent>,
        page: EventsPage,
        context_state: Option<AgentContextState>,
    },
    Stream {
        generation: u64,
        message: StreamMessage,
    },
    Connection {
        generation: u64,
        status: String,
    },
    SessionUpdated(Session),
    /// A model's pin changed on the server.
    ModelPinned {
        model: llm_api::providers::Model,
        pinned: bool,
    },
    /// A draft's agent configurations loaded.
    DraftConfigs {
        generation: u64,
        agents_dir: Option<psa::api::ConfigGroup>,
        configs: Vec<psa::api::AgentConfigStatus>,
        default_name: String,
    },
    DraftConfigsFailed {
        generation: u64,
        message: String,
    },
    /// A draft's session was created; its agent comes next.
    DraftSessionCreated {
        generation: u64,
        session: Session,
    },
    /// A draft's first message was not sent.
    DraftFailed {
        generation: u64,
        message: String,
        text: String,
    },
    /// Per-model usage for `/info`.
    ModelUsage {
        agent_id: AgentId,
        usage: Vec<ModelUsage>,
    },
    /// A message may not have been delivered; offer its text again.
    SendFailed(String),
    ServerRestarted(ConnectionSettings),
    ServerStopped,
    Notice(String),
    Error(String),
    /// Show text in a panel over the transcript.
    Panel {
        title: String,
        lines: Vec<String>,
    },
}

/// The agent whose transcript is shown.
pub struct AgentView {
    pub generation: u64,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub session: Option<Session>,
    pub agents: Vec<Agent>,
    pub transcript: Transcript,
    pub connection: String,
    follow: JoinHandle<()>,
}

impl AgentView {
    pub fn agent(&self) -> Option<&Agent> {
        self.transcript
            .agents
            .get(&self.agent_id)
            .or_else(|| self.agents.iter().find(|agent| agent.id == self.agent_id))
    }

    pub fn session(&self) -> Option<&Session> {
        self.transcript.session.as_ref().or(self.session.as_ref())
    }

    pub fn is_running(&self) -> bool {
        self.agent()
            .is_some_and(|agent| agent.state == AgentState::Running)
    }

    /// Replace the agent's entry in the session's list, or add a new one.
    pub fn update_agent(&mut self, agent: Agent) {
        match self.agents.iter_mut().find(|known| known.id == agent.id) {
            Some(known) => *known = agent,
            None => self.agents.push(agent),
        }
    }

    /// The oldest pending question for this agent.
    pub fn pending_query(&self) -> Option<&ClientQueryRequest> {
        self.transcript.queries.values().next()
    }
}

impl Drop for AgentView {
    fn drop(&mut self) {
        self.follow.abort();
    }
}

/// Answers being collected for an agent's question.
pub struct QuestionDialog {
    pub request: ClientQueryRequest,
    pub index: usize,
    pub answers: Vec<String>,
    pub input: Composer,
    /// Highlighted suggestion, or `None` for the typed answer.
    pub choice: Option<usize>,
}

impl QuestionDialog {
    pub fn new(request: ClientQueryRequest) -> Self {
        Self {
            request,
            index: 0,
            answers: Vec::new(),
            input: Composer::default(),
            choice: None,
        }
    }

    pub fn questions(&self) -> &[psa::api::ClientQuestion] {
        let psa::api::ClientQuery::AskQuestion(request) = &self.request.payload;
        &request.questions
    }
}

/// What a [`PromptDialog`]'s submitted value does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    /// Rename the open session, or name the draft.
    RenameSession,
}

/// A single value typed for a slash command, such as `/rename`'s new name.
pub struct PromptDialog {
    pub action: PromptAction,
    pub title: &'static str,
    pub input: Composer,
}

/// A section of the settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Appearance,
    /// Commands and keys, which `/help` opens.
    Commands,
}

pub enum Overlay {
    Picker(Picker),
    Prompt(PromptDialog),
    /// The settings dialog; `changed` saves the settings when it closes.
    /// `selected` is the highlighted row, or the scroll offset on the
    /// commands tab.
    Settings {
        tab: SettingsTab,
        selected: usize,
        changed: bool,
    },
    Panel {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
    Question(QuestionDialog),
}

pub enum StatusMessage {
    Notice(String),
    Error(String),
}

pub struct App {
    pub network: Network,
    pub settings: TuiSettings,
    pub settings_path: PathBuf,
    pub local_server: Option<Arc<LocalServerManager>>,
    pub server_url: String,
    pub sessions: Vec<Session>,
    pub probes: Vec<Probe>,
    pub models: Vec<ModelInfo>,
    pub view: Option<AgentView>,
    /// A new session not created yet; never open together with `view`.
    pub draft: Option<super::draft::SessionDraft>,
    pub draft_generation: u64,
    /// The menu offers the draft root's model policies, after `/select agent`.
    pub choosing_root_model: bool,
    pub composer: Composer,
    pub completion: CommandSelection,
    /// `/model` lists every model, not just pinned ones.
    pub show_all_models: bool,
    pub overlay: Option<Overlay>,
    pub message: Option<StatusMessage>,
    /// Transcript lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    /// Transcript height at the last draw, for paging.
    pub page_height: usize,
    /// Transcript line count at the last draw, to hold a scrolled view in place.
    pub last_line_count: usize,
    /// The transcript scrollbar's column at the last draw, when it overflows.
    pub scrollbar: Option<Rect>,
    /// The first agent the status bar showed at the last draw, so cycling
    /// only scrolls the bar when the open agent would leave it.
    pub agent_bar_offset: usize,
    /// Show thinking, harness messages, and longer tool output.
    pub details: bool,
    pub quit: bool,
    /// Printed after the terminal is restored.
    pub exit_message: Option<String>,
    /// Ctrl-C was pressed once with an empty input.
    pub quit_armed: bool,
    /// Questions the user closed without answering; Ctrl-Q reopens them.
    pub dismissed_queries: Vec<ClientQueryId>,
    pub tick: u64,
    generation: u64,
}

impl App {
    pub fn new(
        network: Network,
        connection: &ConnectionSettings,
        local_server: Option<LocalServerManager>,
        settings: TuiSettings,
        settings_path: PathBuf,
    ) -> Self {
        Self {
            network,
            settings,
            settings_path,
            local_server: local_server.map(Arc::new),
            server_url: connection.api_url.clone(),
            sessions: Vec::new(),
            probes: Vec::new(),
            models: Vec::new(),
            view: None,
            draft: None,
            draft_generation: 0,
            choosing_root_model: false,
            composer: Composer::default(),
            completion: CommandSelection::default(),
            show_all_models: false,
            overlay: None,
            message: None,
            scroll: 0,
            page_height: 10,
            last_line_count: 0,
            scrollbar: None,
            agent_bar_offset: 0,
            details: false,
            quit: false,
            exit_message: None,
            quit_armed: false,
            dismissed_queries: Vec::new(),
            tick: 0,
            generation: 0,
        }
    }

    /// Load the catalog and start a new session in the current directory;
    /// `/sessions` opens an existing one.
    pub fn start(&mut self) {
        self.network.load_catalog();
        self.start_draft();
    }

    pub fn palette(&self) -> Palette {
        self.settings.preferences.theme.palette()
    }

    /// The preferences the transcript uses, with everything shown in details view.
    pub fn display_preferences(&self) -> TuiPreferences {
        if self.details {
            self.settings.preferences.with_details()
        } else {
            self.settings.preferences.clone()
        }
    }

    /// Close the settings dialog, saving any change in the background.
    pub fn close_settings(&mut self) {
        let Some(Overlay::Settings { changed, .. }) = self.overlay.take() else {
            return;
        };
        if !changed {
            return;
        }
        let path = self.settings_path.clone();
        let settings = self.settings.clone();
        let events = self.network.events();
        tokio::spawn(async move {
            if let Err(error) = super::settings::save_settings(&path, &settings).await {
                let _ = events.send(AppEvent::Error(error.to_string()));
            }
        });
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.message = Some(StatusMessage::Notice(message.into()));
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.message = Some(StatusMessage::Error(message.into()));
    }

    /// Show `agent_id`'s transcript, following its events.
    pub fn open_agent(&mut self, session_id: SessionId, agent_id: AgentId) {
        self.draft = None;
        self.choosing_root_model = false;
        self.generation += 1;
        let follow = self
            .network
            .follow_agent(self.generation, session_id, agent_id);
        let session = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .cloned();
        self.view = Some(AgentView {
            generation: self.generation,
            session_id,
            agent_id,
            session,
            agents: Vec::new(),
            transcript: Transcript::new(session_id, Some(agent_id)),
            connection: "connecting".into(),
            follow,
        });
        self.scroll = 0;
        self.dismissed_queries.clear();
    }

    /// An open agent, a draft, or neither.
    pub fn command_context(&self) -> CommandContext {
        match (&self.view, &self.draft) {
            (Some(_), _) => CommandContext::Agent,
            (None, Some(_)) => CommandContext::SessionDraft,
            (None, None) => CommandContext::NoSession,
        }
    }

    pub fn status(&self) -> client_shared::AgentStatus {
        self.view
            .as_ref()
            .map(|view| agent_status(&view.transcript, &self.models))
            .unwrap_or_default()
    }

    /// Whether the screen changes without new events, such as a spinner.
    pub fn is_animating(&self) -> bool {
        self.view.as_ref().is_some_and(AgentView::is_running)
            || self.draft.as_ref().is_some_and(|draft| draft.pending)
    }

    pub fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::Sessions(mut sessions) => {
                sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
                self.sessions = sessions;
                self.fill_session_picker();
            }
            AppEvent::Probes(probes) => {
                self.probes = probes;
                self.apply_draft_probes();
            }
            AppEvent::DraftConfigs {
                generation,
                agents_dir,
                configs,
                default_name,
            } => self.apply_draft_configs(generation, agents_dir, configs, &default_name),
            AppEvent::DraftConfigsFailed {
                generation,
                message,
            } => self.fail_draft_configs(generation, message),
            AppEvent::DraftSessionCreated {
                generation,
                session,
            } => self.apply_draft_session(generation, session),
            AppEvent::DraftFailed {
                generation,
                message,
                text,
            } => self.fail_draft(generation, message, text),
            AppEvent::Models(models) => self.models = models,
            AppEvent::ProbeReport { probes, sandboxes } => {
                let lines = super::commands::describe_probes(&probes, &sandboxes);
                self.overlay = Some(Overlay::Panel {
                    title: "Probes".into(),
                    lines,
                    scroll: 0,
                });
            }
            AppEvent::Agents { session_id, agents } => self.apply_agents(session_id, agents),
            AppEvent::OpenAgent {
                session_id,
                agent_id,
            } => {
                if matches!(self.message, Some(StatusMessage::Notice(_))) {
                    self.message = None;
                }
                self.open_agent(session_id, agent_id);
            }
            AppEvent::Opened {
                generation,
                session,
                agents,
                page,
                context_state,
            } => {
                let Some(view) = self.view.as_mut().filter(|v| v.generation == generation) else {
                    return;
                };
                view.session = Some(session);
                view.agents = agents;
                view.transcript.bootstrap(page);
                view.transcript.context_state = context_state;
                view.connection = "live".into();
                self.open_pending_question();
            }
            AppEvent::Stream {
                generation,
                message,
            } => {
                let Some(view) = self.view.as_mut().filter(|v| v.generation == generation) else {
                    return;
                };
                match &message {
                    StreamMessage::RequestFailed {
                        agent_id, message, ..
                    } if *agent_id == view.agent_id => {
                        self.message = Some(StatusMessage::Error(message.clone()));
                    }
                    StreamMessage::AgentUpdated { agent }
                        if agent.session_id == view.session_id =>
                    {
                        view.update_agent(agent.clone());
                    }
                    _ => {}
                }
                view.transcript.apply(message);
                self.open_pending_question();
            }
            AppEvent::Connection { generation, status } => {
                if let Some(view) = self.view.as_mut().filter(|v| v.generation == generation) {
                    view.connection = status;
                }
            }
            AppEvent::SessionUpdated(session) => {
                if let Some(view) = self.view.as_mut().filter(|v| v.session_id == session.id) {
                    view.session = Some(session.clone());
                    view.transcript.session = Some(session.clone());
                }
                if let Some(old) = self.sessions.iter_mut().find(|old| old.id == session.id) {
                    *old = session;
                }
            }
            AppEvent::ModelPinned { model, pinned } => {
                if let Some(info) = self.models.iter_mut().find(|info| info.model == model) {
                    info.pinned = pinned;
                }
                self.refresh_pin_picker();
            }
            AppEvent::ModelUsage { agent_id, usage } => {
                let open = self
                    .view
                    .as_ref()
                    .is_some_and(|view| view.agent_id == agent_id);
                if open && self.is_info_open() {
                    let lines = self.describe_agent(Some(&usage));
                    if let Some(Overlay::Panel { lines: shown, .. }) = &mut self.overlay {
                        *shown = lines;
                    }
                }
            }
            AppEvent::SendFailed(text) => {
                if self.composer.is_empty() {
                    self.composer.set_text(text);
                }
            }
            AppEvent::ServerRestarted(connection) => self.reconnect(connection),
            AppEvent::ServerStopped => {
                self.exit_message = Some("Local server stopped.".into());
                self.quit = true;
            }
            AppEvent::Notice(message) => self.set_notice(message),
            AppEvent::Error(message) => self.set_error(message),
            AppEvent::Panel { title, lines } => {
                self.overlay = Some(Overlay::Panel {
                    title,
                    lines,
                    scroll: 0,
                });
            }
        }
    }

    fn fill_session_picker(&mut self) {
        let Some(Overlay::Picker(picker)) = &mut self.overlay else {
            return;
        };
        if !picker.loading || picker.title != SESSIONS_TITLE {
            return;
        }
        let items = super::commands::session_items(&self.sessions, &self.probes);
        if items.is_empty() {
            self.overlay = None;
            self.set_notice("No sessions yet. Type /new to start one.");
            return;
        }
        picker.set_items(items);
    }

    fn apply_agents(&mut self, session_id: SessionId, agents: Vec<Agent>) {
        let Some(view) = self.view.as_mut().filter(|v| v.session_id == session_id) else {
            return;
        };
        view.agents = agents;
        let Some(Overlay::Picker(picker)) = &mut self.overlay else {
            return;
        };
        if picker.loading && picker.title == AGENTS_TITLE {
            let show_hidden = self.settings.preferences.show_hidden_agents;
            picker.set_items(super::commands::agent_items(&view.agents, show_hidden));
        }
    }

    /// Open the agent's question unless another dialog is in the way or the
    /// user closed it.
    fn open_pending_question(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        let Some(query) = self.view.as_ref().and_then(AgentView::pending_query) else {
            return;
        };
        if self.dismissed_queries.contains(&query.id) {
            return;
        }
        self.overlay = Some(Overlay::Question(QuestionDialog::new(query.clone())));
    }

    /// Reopen a question the user closed.
    pub fn reopen_question(&mut self) {
        self.dismissed_queries.clear();
        self.open_pending_question();
        if self.overlay.is_none() {
            self.set_notice("No questions are waiting.");
        }
    }

    /// Switch to a restarted server, keeping the open agent.
    fn reconnect(&mut self, connection: ConnectionSettings) {
        match psa::api::Client::new(&connection.api_url, &connection.client_key) {
            Ok(client) => self.network.set_client(client),
            Err(error) => {
                self.set_error(format!("Unable to reconnect: {error}"));
                return;
            }
        }
        self.server_url = connection.api_url;
        self.set_notice("Local server restarted.");
        self.network.load_catalog();
        if let Some((session_id, agent_id)) = self.view.as_ref().map(|v| (v.session_id, v.agent_id))
        {
            self.open_agent(session_id, agent_id);
        }
        self.load_draft_configs();
    }
}
