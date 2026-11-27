//! A new session that exists only in the TUI until its first message, which
//! creates the session, its root agent, and then sends the message.
use super::{
    app::{App, Overlay, PromptAction, PromptDialog},
    commands::describe_probe_name,
    composer::Composer,
};
use client_shared::{
    CommandId, DraftRoot, build_effort_config, describe_config_group, draft_model_label,
    name_from_message,
};
use llm_api::providers::{Effort, ModelConfig};
use psa::api::{
    AgentConfigStatus, ConfigGroup, ConfigId, CreateSession, ModelSelection, ProbeId, Session,
};
use std::path::PathBuf;

/// Everything the first message needs to create the session and its agent.
pub struct SessionDraft {
    /// Ignores configuration loads and creation results for an older draft.
    pub generation: u64,
    /// `None` until a connected probe is known.
    pub probe_id: Option<ProbeId>,
    pub directory: PathBuf,
    /// Set by `/rename`; otherwise the first message names the session.
    pub name: Option<String>,
    /// Set by `/model` and `/effort`; otherwise the server's default is used.
    pub model_config: Option<ModelConfig>,
    pub root: DraftRoot,
    /// The directory's `.agents` group, once configurations are loaded.
    pub agents_dir: Option<ConfigGroup>,
    /// A session an earlier attempt created before its agent failed; the next
    /// attempt reuses it rather than creating another.
    pub session: Option<Session>,
    /// The first message is being sent.
    pub pending: bool,
}

/// What the network task needs to turn a draft into a session and agent.
pub struct DraftSubmission {
    pub generation: u64,
    pub session: Option<Session>,
    pub create: CreateSession,
    pub config_id: ConfigId,
    pub model: Option<ModelSelection>,
    pub text: String,
}

impl App {
    /// `/new` and startup: a draft in the open session's probe and
    /// directory, or this machine's host probe and the current directory.
    pub fn start_draft(&mut self) {
        let location = match (&self.view, &self.draft) {
            (Some(view), _) => view
                .session()
                .map(|session| (Some(session.probe_id), session.working_directory.clone())),
            (None, Some(draft)) => Some((draft.probe_id, draft.directory.clone())),
            (None, None) => None,
        };
        let (probe_id, directory) = location.unwrap_or_else(|| {
            let directory = std::env::current_dir().unwrap_or_else(|_| "/".into());
            (self.host_probe(), directory)
        });

        self.view = None;
        self.choosing_root_model = false;
        self.draft_generation += 1;
        self.draft = Some(SessionDraft {
            generation: self.draft_generation,
            probe_id,
            directory,
            name: None,
            model_config: None,
            root: DraftRoot::default(),
            agents_dir: None,
            session: None,
            pending: false,
        });
        self.scroll = 0;
        self.load_draft_configs();
    }

    /// A connected probe on this machine, or any connected probe.
    fn host_probe(&self) -> Option<ProbeId> {
        self.probes
            .iter()
            .find(|probe| probe.metadata.host && probe.is_active)
            .or_else(|| self.probes.iter().find(|probe| probe.is_active))
            .map(|probe| probe.metadata.id)
    }

    /// Give a draft started before probes loaded the host probe.
    pub fn apply_draft_probes(&mut self) {
        let host = self.host_probe();
        let Some(draft) = self.draft.as_mut().filter(|draft| draft.probe_id.is_none()) else {
            return;
        };
        draft.probe_id = host;
        self.load_draft_configs();
    }

    pub fn load_draft_configs(&mut self) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let Some(probe_id) = draft.probe_id else {
            return;
        };
        draft.root.loading = true;
        draft.root.ready = false;
        let directory = draft.directory.clone();
        self.network
            .load_draft_configs(draft.generation, probe_id, directory);
    }

    /// Offer the global configurations and the directory's own `.agents` ones.
    pub fn apply_draft_configs(
        &mut self,
        generation: u64,
        agents_dir: Option<ConfigGroup>,
        configs: Vec<AgentConfigStatus>,
        default_name: &str,
    ) {
        let Some(draft) = self.draft.as_mut().filter(|d| d.generation == generation) else {
            return;
        };
        let configs = configs
            .into_iter()
            .filter(|status| match &status.config.config_id.config_group {
                ConfigGroup::AgentsDir { .. } => {
                    Some(&status.config.config_id.config_group) == agents_dir.as_ref()
                }
                _ => true,
            })
            .collect();
        draft.agents_dir = agents_dir;
        draft.root.apply_configs(configs, default_name);
        draft.root.loading = false;
        draft.root.ready = true;
    }

    pub fn fail_draft_configs(&mut self, generation: u64, message: String) {
        if let Some(draft) = self.draft.as_mut().filter(|d| d.generation == generation) {
            draft.root.loading = false;
        }
        self.set_error(message);
    }

    /// Commands that change or show the draft; false for commands that act
    /// the same without one.
    pub fn run_draft_command(&mut self, id: CommandId, arguments: &str) -> bool {
        if self.draft.is_none() {
            return false;
        }
        match id {
            CommandId::Rename => self.rename_draft(arguments.to_owned()),
            CommandId::Refresh => {
                self.load_draft_configs();
                self.set_notice("Reloading agent configurations…");
            }
            CommandId::Info => {
                let lines = self.describe_draft();
                self.overlay = Some(Overlay::Panel {
                    title: "New session".into(),
                    lines,
                    scroll: 0,
                });
            }
            // Reached only with the menu dismissed: reopen it on the typed text.
            CommandId::SessionModel
            | CommandId::SessionEffort
            | CommandId::Agent
            | CommandId::Probe => {
                self.composer
                    .set_text(format!("/{} {arguments}", id.spec().name));
            }
            _ => return false,
        }
        true
    }

    fn rename_draft(&mut self, name: String) {
        let draft = self.draft.as_mut().unwrap();
        if name.is_empty() {
            let mut input = Composer::default();
            input.set_text(draft.name.clone().unwrap_or_default());
            self.overlay = Some(Overlay::Prompt(PromptDialog {
                action: PromptAction::RenameSession,
                title: "Name the new session",
                input,
            }));
            return;
        }
        draft.name = Some(name.clone());
        self.set_notice(format!("The session will be named {name}."));
    }

    pub fn set_draft_model(&mut self, config: ModelConfig) {
        let slug = config.model.slug();
        self.draft.as_mut().unwrap().model_config = Some(config);
        self.set_notice(format!("Model set to {slug}."));
    }

    pub fn set_draft_effort(&mut self, effort: Effort) {
        let current = self.draft.as_ref().unwrap().model_config.as_ref();
        let Some(config) = build_effort_config(&self.models, current, effort) else {
            self.set_error("Choose a model with /model first.");
            return;
        };
        self.draft.as_mut().unwrap().model_config = Some(config);
        self.set_notice(format!("Effort set to {}.", effort.as_str()));
    }

    /// `/agent`'s choice; a configuration with several model policies asks
    /// for one next.
    pub fn choose_draft_config(&mut self, id: ConfigId) {
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        draft.root.select_config(id);
        self.choosing_root_model =
            draft.root.model.is_none() && draft.root.model_choices().len() > 1;
    }

    pub fn choose_draft_root_model(&mut self, model: ModelSelection) {
        if let Some(draft) = self.draft.as_mut() {
            draft.root.model = Some(model);
        }
        self.choosing_root_model = false;
    }

    /// `/probe`: the same directory on the same probe, otherwise the probe's
    /// home directory.
    pub fn choose_draft_probe(&mut self, probe_id: ProbeId) {
        let Some(probe) = self.probes.iter().find(|p| p.metadata.id == probe_id) else {
            return;
        };
        let home = probe.metadata.home_dir.clone().map(PathBuf::from);
        let name = describe_probe_name(probe);
        let draft = self.draft.as_mut().unwrap();
        if draft.probe_id != Some(probe_id) {
            draft.directory = home.unwrap_or_else(|| "/".into());
        }
        draft.probe_id = Some(probe_id);
        self.set_notice(format!("The session will start on {name}."));
        self.load_draft_configs();
    }

    /// Send the first message: create the session (unless an earlier attempt
    /// did), then the root agent, then the message.
    pub fn submit_draft(&mut self, text: String) {
        let draft = self.draft.as_ref().unwrap();
        if draft.pending {
            return;
        }
        let Some(probe_id) = draft.probe_id else {
            self.set_error("No connected probe to start the session on; see /probe.");
            return;
        };
        let Some(config_id) = draft.root.selected.clone() else {
            let message = if draft.root.ready {
                "Choose the root agent first: /agent."
            } else {
                "Agent configurations are still loading; try again."
            };
            self.set_error(message);
            return;
        };
        if draft.root.model.is_none() && draft.root.model_choices().len() > 1 {
            self.choosing_root_model = true;
            self.set_error("Choose the root agent's model policy first.");
            return;
        }

        let name = draft
            .name
            .clone()
            .unwrap_or_else(|| name_from_message(&text));
        let submission = DraftSubmission {
            generation: draft.generation,
            session: draft.session.clone(),
            create: CreateSession {
                name,
                working_directory: draft.directory.clone(),
                probe_id,
                project_id: None,
                model_config: draft.model_config.clone(),
                decision_model: None,
            },
            config_id,
            model: draft.root.model.clone(),
            text,
        };
        self.draft.as_mut().unwrap().pending = true;
        self.clear_composer();
        self.set_notice("Starting the session…");
        self.network.submit_draft(submission);
    }

    pub fn apply_draft_session(&mut self, generation: u64, session: Session) {
        if let Some(draft) = self.draft.as_mut().filter(|d| d.generation == generation) {
            draft.session = Some(session);
        }
    }

    /// The first message was not sent; offer its text again.
    pub fn fail_draft(&mut self, generation: u64, message: String, text: String) {
        let Some(draft) = self.draft.as_mut().filter(|d| d.generation == generation) else {
            return;
        };
        draft.pending = false;
        if self.composer.is_empty() {
            self.composer.set_text(text);
        }
        self.set_error(message);
    }

    /// The draft's details, shown in place of a transcript and by `/info`.
    pub fn describe_draft(&self) -> Vec<String> {
        let Some(draft) = &self.draft else {
            return Vec::new();
        };
        let probe = match draft.probe_id {
            Some(id) => self
                .probes
                .iter()
                .find(|probe| probe.metadata.id == id)
                .map(describe_probe_name)
                .unwrap_or_else(|| id.to_string()),
            None => "waiting for a connected probe".into(),
        };
        let agent = match (&draft.root.selected, draft.root.loading) {
            (Some(id), _) => match &id.config_group {
                ConfigGroup::Global => id.name.clone(),
                group => format!("{} ({})", id.name, describe_config_group(group)),
            },
            (None, true) => "loading…".into(),
            (None, false) => "none chosen; see /agent".into(),
        };
        let policy = match &draft.root.model {
            Some(model) => draft_model_label(model),
            None if draft.root.model_choices().len() > 1 => "not chosen; see /agent".into(),
            None => "the configuration's default".into(),
        };
        let model = match &draft.model_config {
            Some(config) => format!("{} · {}", config.model.slug(), config.effort.as_str()),
            None => "the server's default".into(),
        };
        let mut lines = vec![
            format!(
                "Name:        {}",
                draft
                    .name
                    .as_deref()
                    .unwrap_or("from the first message (see /rename)")
            ),
            format!("Probe:       {probe}"),
            format!("Directory:   {}", draft.directory.display()),
            format!("Root agent:  {agent}"),
            format!("Agent model: {policy}"),
            format!("Session model: {model}"),
        ];
        if draft.session.is_some() {
            lines.push("The session exists; the next message creates its agent.".into());
        }
        lines
    }
}

#[cfg(test)]
#[path = "../tests/tui/draft/tests.rs"]
mod tests;
