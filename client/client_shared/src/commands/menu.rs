//! The slash command completion menu: which choices the composer offers for
//! what is typed, and what Tab and Enter do with them. Each client draws the
//! menu and maps keys to it; the choices and their meaning are shared.
use super::{
    Arguments, CommandContext, CommandId, CommandSpec, Frontend, SubcommandSet, complete_commands,
    split_command_line,
};
use crate::{DraftRoot, draft_model_label};
use llm_api::providers::{Effort, ModelConfig};
use psa::api::{ConfigGroup, ConfigId, ModelInfo, ModelKind, ModelSelection, Probe, ProbeId};

/// What a client knows when it builds the menu.
#[derive(Debug, Clone, Copy)]
pub struct MenuContext<'a> {
    pub frontend: Frontend,
    pub context: CommandContext,
    pub models: &'a [ModelInfo],
    /// The session's default model and effort, once the session has loaded.
    pub session_model: Option<&'a ModelConfig>,
    /// `/model` also lists models that are not pinned.
    pub show_all_models: bool,
    /// The client is connected to a server it can manage with `/server`.
    pub local_server: bool,
    /// The root agent being chosen, before it exists.
    pub root: Option<&'a DraftRoot>,
    /// Probes `/select probe` offers.
    pub probes: &'a [Probe],
}

impl MenuContext<'_> {
    /// Whether `spec` can run here.
    pub fn offers(&self, spec: &CommandSpec) -> bool {
        spec.supports(self.frontend)
            && spec.scope.allows(self.context)
            && (spec.id != CommandId::Server || self.local_server)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandMenu {
    /// The command whose arguments are offered; `None` while completing a name.
    pub command: Option<CommandId>,
    pub title: &'static str,
    /// A line about the current value, such as the session's model.
    pub note: Option<String>,
    pub candidates: Vec<Candidate>,
    /// Shown when nothing can be chosen.
    pub empty_hint: &'static str,
}

impl CommandMenu {
    /// Whether anything can be chosen; a toggle alone leaves the menu empty.
    pub fn has_choices(&self) -> bool {
        self.candidates
            .iter()
            .any(|candidate| candidate.kind == CandidateKind::Choice)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Choice,
    /// Changes what the menu lists rather than choosing; Tab and Enter both
    /// apply it and the menu stays open.
    Toggle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub label: String,
    pub detail: String,
    pub kind: CandidateKind,
    /// The composer text Tab writes, if Tab completes this choice.
    pub completion: Option<String>,
    /// What Enter does.
    pub action: MenuAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    /// Replace the composer text, such as `/model ` to open the model menu.
    Fill(String),
    /// Run a command, as if `/name arguments` were sent.
    Run {
        id: CommandId,
        arguments: String,
    },
    SetModel(ModelConfig),
    SetEffort(Effort),
    ToggleAllModels,
    /// Choose the root agent's configuration; a model policy may follow.
    ChooseConfig(ConfigId),
    ChooseRootModel(ModelSelection),
    ChooseProbe(ProbeId),
}

/// The menu for `input`, or `None` when no menu applies and Enter sends the
/// input as typed.
pub fn build_command_menu(input: &str, cx: &MenuContext) -> Option<CommandMenu> {
    let rest = input.strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let Some((spec, arguments)) =
        split_command_line(rest, cx.frontend).filter(|(_, arguments)| !arguments.is_empty())
    else {
        return build_name_menu(input, cx);
    };

    if !cx.offers(spec) {
        return None;
    }
    match spec.arguments {
        Arguments::Subcommands(set) => build_subcommand_menu(spec, set, arguments),
        Arguments::Models => Some(build_model_menu(arguments, cx)),
        Arguments::Efforts => Some(build_effort_menu(arguments, cx)),
        Arguments::Configurations => Some(build_configuration_menu(arguments, cx)),
        Arguments::Probes => Some(build_probe_menu(arguments, cx)),
        Arguments::None | Arguments::Text(_) => None,
    }
}

fn build_name_menu(input: &str, cx: &MenuContext) -> Option<CommandMenu> {
    let candidates: Vec<Candidate> = complete_commands(input, cx.frontend)
        .into_iter()
        .filter(|spec| cx.offers(spec))
        .map(|spec| Candidate {
            label: format!("/{}", spec.name),
            detail: spec.description.into(),
            kind: CandidateKind::Choice,
            completion: Some(spec.completion()),
            action: if spec.has_argument_menu() {
                MenuAction::Fill(format!("/{} ", spec.name))
            } else {
                MenuAction::Run {
                    id: spec.id,
                    arguments: String::new(),
                }
            },
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    Some(CommandMenu {
        command: None,
        title: "Commands",
        note: None,
        candidates,
        empty_hint: "No matching commands.",
    })
}

/// Subcommands matching the typed word. Once a subcommand is followed by more
/// text, or nothing matches, the menu closes so the text is sent as typed.
fn build_subcommand_menu(
    spec: &CommandSpec,
    set: SubcommandSet,
    arguments: &str,
) -> Option<CommandMenu> {
    let typed = arguments.trim_start();
    if typed.contains(char::is_whitespace) {
        return None;
    }
    let typed = typed.to_lowercase();

    let mut candidates = Vec::new();
    if let Some(description) = set.bare.filter(|_| typed.is_empty()) {
        candidates.push(Candidate {
            label: "show".into(),
            detail: description.into(),
            kind: CandidateKind::Choice,
            completion: None,
            action: MenuAction::Run {
                id: spec.id,
                arguments: String::new(),
            },
        });
    }
    for option in set.options {
        if !option.name.starts_with(&typed) {
            continue;
        }
        let space = if option.text.is_some() { " " } else { "" };
        candidates.push(Candidate {
            label: option.name.into(),
            detail: option.description.into(),
            kind: CandidateKind::Choice,
            completion: Some(format!("/{} {}{space}", spec.name, option.name)),
            action: MenuAction::Run {
                id: spec.id,
                arguments: option.name.into(),
            },
        });
    }
    if candidates.is_empty() {
        return None;
    }
    Some(CommandMenu {
        command: Some(spec.id),
        title: subcommand_title(spec.id),
        note: set
            .text
            .map(|text| format!("Or type the {text} after /{}", spec.name)),
        candidates,
        empty_hint: "No matching options.",
    })
}

fn subcommand_title(id: CommandId) -> &'static str {
    match id {
        CommandId::Goal => "Agent goal",
        CommandId::Sandbox => "Session sandbox",
        CommandId::Server => "Local server",
        _ => "Options",
    }
}

/// Pinned models and the current one, or every chat model once toggled; the
/// typed words narrow what is listed. The toggle comes last.
fn build_model_menu(arguments: &str, cx: &MenuContext) -> CommandMenu {
    let words: Vec<String> = arguments
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let current = cx.session_model;
    let is_current = |model: &ModelInfo| current.is_some_and(|config| config.model == model.model);
    let is_listed = |model: &ModelInfo| model.pinned || is_current(model);

    let mut matching: Vec<&ModelInfo> = cx
        .models
        .iter()
        .filter(|model| model.kind == ModelKind::Chat && matches_words(model, &words))
        .collect();
    // Pinned first, then the current model, then the rest.
    matching.sort_by_key(|model| (!model.pinned, !is_current(model)));
    let hidden = matching.iter().filter(|model| !is_listed(model)).count();
    let preferred = current
        .map(|config| config.effort)
        .unwrap_or(Effort::Medium);

    let mut candidates: Vec<Candidate> = matching
        .iter()
        .filter(|model| cx.show_all_models || is_listed(model))
        .map(|model| {
            let mut detail = model.model.slug();
            if is_current(model) {
                detail.push_str(" · current");
            }
            Candidate {
                label: model.name.clone(),
                detail,
                kind: CandidateKind::Choice,
                completion: Some(format!("/model {}", model.name)),
                action: MenuAction::SetModel(ModelConfig {
                    model: model.model.clone(),
                    effort: choose_effort(model, preferred),
                }),
            }
        })
        .collect();

    let unlisted = cx
        .models
        .iter()
        .any(|model| model.kind == ModelKind::Chat && !is_listed(model));
    if unlisted {
        let label = match (cx.show_all_models, hidden) {
            (true, _) => "Show pinned only".to_owned(),
            (false, 0) => "Show all models".to_owned(),
            (false, _) if words.is_empty() => format!("Show all models ({hidden} more)"),
            (false, _) => format!("Show all models ({hidden} more match)"),
        };
        candidates.push(Candidate {
            label,
            detail: String::new(),
            kind: CandidateKind::Toggle,
            completion: None,
            action: MenuAction::ToggleAllModels,
        });
    }

    CommandMenu {
        command: Some(CommandId::SessionModel),
        title: "Session default model (not individual agents)",
        note: current.map(|config| {
            format!(
                "Current: {} · {}",
                config.model.slug(),
                config.effort.as_str()
            )
        }),
        candidates,
        empty_hint: if cx.show_all_models || !words.is_empty() {
            "No matching models."
        } else {
            "No pinned models. Pin models in settings."
        },
    }
}

fn matches_words(model: &ModelInfo, words: &[String]) -> bool {
    let text = format!("{} {}", model.name, model.model.slug()).to_lowercase();
    words.iter().all(|word| text.contains(word.as_str()))
}

/// The efforts the session's current model supports, narrowed by the typed text.
fn build_effort_menu(arguments: &str, cx: &MenuContext) -> CommandMenu {
    let filter = arguments.trim().to_lowercase();
    let candidates = session_efforts(cx.models, cx.session_model)
        .into_iter()
        .filter(|effort| effort.as_str().contains(&filter))
        .map(|effort| Candidate {
            label: effort.as_str().into(),
            detail: String::new(),
            kind: CandidateKind::Choice,
            completion: Some(format!("/effort {}", effort.as_str())),
            action: MenuAction::SetEffort(effort),
        })
        .collect();
    CommandMenu {
        command: Some(CommandId::SessionEffort),
        title: "Session default effort (not individual agents)",
        note: cx
            .session_model
            .map(|config| format!("Current: {}", config.effort.as_str())),
        candidates,
        empty_hint: if cx.session_model.is_none() {
            "Choose a model with /model first."
        } else {
            "No matching effort options for the current model."
        },
    }
}

/// `/select agent`: the root agent configurations, narrowed by the typed text.
fn build_configuration_menu(arguments: &str, cx: &MenuContext) -> CommandMenu {
    let filter = arguments.trim().to_lowercase();
    let configs = cx
        .root
        .map(|root| root.configs.as_slice())
        .unwrap_or_default();
    let selected = cx.root.and_then(|root| root.selected.as_ref());
    let candidates = configs
        .iter()
        .filter(|status| {
            status
                .config
                .config_id
                .name
                .to_lowercase()
                .contains(&filter)
        })
        .map(|status| {
            let config = &status.config;
            let mut detail = describe_config_group(&config.config_id.config_group);
            if !config.description.is_empty() {
                detail.push_str(" · ");
                detail.push_str(&config.description);
            }
            if selected == Some(&config.config_id) {
                detail.push_str(" · current");
            }
            Candidate {
                label: config.config_id.name.clone(),
                detail,
                kind: CandidateKind::Choice,
                completion: Some(format!("/select agent {}", config.config_id.name)),
                action: MenuAction::ChooseConfig(config.config_id.clone()),
            }
        })
        .collect();
    CommandMenu {
        command: Some(CommandId::SelectAgent),
        title: "Choose a root agent",
        note: None,
        candidates,
        empty_hint: if cx.root.is_some_and(|root| root.ready) {
            "No matching usable configurations."
        } else {
            "Loading configurations…"
        },
    }
}

/// The chosen configuration's model policies, offered after `/select agent` when it
/// has more than one.
pub fn build_root_model_menu(root: &DraftRoot) -> CommandMenu {
    let candidates = root
        .model_choices()
        .into_iter()
        .map(|selection| Candidate {
            label: draft_model_label(&selection),
            detail: String::new(),
            kind: CandidateKind::Choice,
            completion: None,
            action: MenuAction::ChooseRootModel(selection),
        })
        .collect();
    CommandMenu {
        command: Some(CommandId::SelectAgent),
        title: "Choose the root agent model policy",
        note: None,
        candidates,
        empty_hint: "No matching options.",
    }
}

/// `/select probe`: connected probes, narrowed by the typed words.
fn build_probe_menu(arguments: &str, cx: &MenuContext) -> CommandMenu {
    let words: Vec<String> = arguments
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let candidates = cx
        .probes
        .iter()
        .filter(|probe| probe.is_active)
        .filter(|probe| {
            let text = format!(
                "{} {}",
                probe.metadata.label.as_deref().unwrap_or_default(),
                probe.metadata.hostname
            )
            .to_lowercase();
            words.iter().all(|word| text.contains(word.as_str()))
        })
        .map(|probe| {
            let name = describe_probe(probe);
            let mut detail = format!("{}/{}", probe.metadata.os, probe.metadata.architecture);
            if probe.metadata.host {
                detail.push_str(" · this machine");
            }
            Candidate {
                completion: Some(format!("/select probe {name}")),
                label: name,
                detail,
                kind: CandidateKind::Choice,
                action: MenuAction::ChooseProbe(probe.metadata.id),
            }
        })
        .collect();
    CommandMenu {
        command: Some(CommandId::SelectProbe),
        title: "Start the session on",
        note: None,
        candidates,
        empty_hint: "No matching connected probes.",
    }
}

/// A probe's label, or its hostname.
pub fn describe_probe(probe: &Probe) -> String {
    probe
        .metadata
        .label
        .clone()
        .unwrap_or_else(|| probe.metadata.hostname.clone())
}

/// Where a configuration was found, as menus show it.
pub fn describe_config_group(group: &ConfigGroup) -> String {
    match group {
        ConfigGroup::Global => "global".into(),
        ConfigGroup::AgentsDir { path, .. } => path.clone(),
        ConfigGroup::Custom(name) => name.clone(),
        other => format!("{other:?}"),
    }
}

/// Efforts the session's current model supports, in catalog order.
pub fn session_efforts(models: &[ModelInfo], current: Option<&ModelConfig>) -> Vec<Effort> {
    current
        .and_then(|config| models.iter().find(|model| model.model == config.model))
        .map(|model| {
            model
                .reasoning_options
                .iter()
                .map(|option| option.effort)
                .collect()
        })
        .unwrap_or_default()
}

/// The session's model config with `effort`, if its model supports it.
pub fn build_effort_config(
    models: &[ModelInfo],
    current: Option<&ModelConfig>,
    effort: Effort,
) -> Option<ModelConfig> {
    if !session_efforts(models, current).contains(&effort) {
        return None;
    }
    current.cloned().map(|mut config| {
        config.effort = effort;
        config
    })
}

/// `preferred` if `model` supports it, otherwise the model's first effort.
pub fn choose_effort(model: &ModelInfo, preferred: Effort) -> Effort {
    if model
        .reasoning_options
        .iter()
        .any(|option| option.effort == preferred)
    {
        return preferred;
    }
    model
        .reasoning_options
        .first()
        .map(|option| option.effort)
        .unwrap_or(Effort::Off)
}

#[cfg(test)]
#[path = "../tests/commands/menu/tests.rs"]
mod tests;
