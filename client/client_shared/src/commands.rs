//! Client commands are UI actions, never accidentally forwarded as prompt text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    SessionModel,
    SessionEffort,
    Agent,
    Refresh,
    Info,
    Compact,
    Rename,
    Fork,
    Sandbox,
}

impl CommandId {
    /// Root drafts have no agent yet; existing agents can no longer choose one.
    pub fn is_available(self, root: bool) -> bool {
        match self {
            Self::Agent => root,
            Self::Compact | Self::Fork => !root,
            Self::SessionModel
            | Self::SessionEffort
            | Self::Info
            | Self::Rename
            | Self::Refresh
            | Self::Sandbox => true,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: CommandId,
    pub name: &'static str,
    pub description: &'static str,
    /// Arguments are chosen from a menu: `/name ` opens it, with arrow keys,
    /// Tab completion, and Enter to select.
    pub picker: bool,
}
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::SessionModel,
        name: "model",
        description: "Change this session's default model and effort",
        picker: true,
    },
    CommandSpec {
        id: CommandId::Agent,
        name: "agent",
        description: "Choose the root agent and its model policy",
        picker: true,
    },
    CommandSpec {
        id: CommandId::Refresh,
        name: "refresh",
        description: "Reload agent and skill configurations from disk",
        picker: false,
    },
    CommandSpec {
        id: CommandId::SessionEffort,
        name: "effort",
        description: "Change this session's default reasoning effort",
        picker: true,
    },
    CommandSpec {
        id: CommandId::Info,
        name: "info",
        description: "Show agent information and token usage",
        picker: false,
    },
    CommandSpec {
        id: CommandId::Compact,
        name: "compact",
        description: "Summarize this agent's older context now; optional focus instructions",
        picker: false,
    },
    CommandSpec {
        id: CommandId::Rename,
        name: "rename",
        description: "Rename this session",
        picker: false,
    },
    CommandSpec {
        id: CommandId::Fork,
        name: "fork",
        description: "Fork this session at its current point; optional new name",
        picker: false,
    },
    CommandSpec {
        id: CommandId::Sandbox,
        name: "sandbox",
        description: "Start or stop this session's sandbox",
        picker: true,
    },
];

/// Lifecycle change to a sandbox from the probes pane or a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxChange {
    Start,
    Stop,
    Delete,
}

impl SandboxChange {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Delete => "delete",
        }
    }

    pub fn past_tense(self) -> &'static str {
        match self {
            Self::Start => "started",
            Self::Stop => "stopped",
            Self::Delete => "deleted",
        }
    }
}

/// Lifecycle action for the current session's sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAction {
    Start,
    Stop,
}

impl SandboxAction {
    pub const ALL: [Self; 2] = [Self::Start, Self::Stop];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Start => "Start this session's sandbox container",
            Self::Stop => "Stop this session's sandbox container",
        }
    }

    /// Parse `/sandbox` arguments; `None` for empty or unrecognized actions.
    pub fn parse(arguments: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| arguments.trim().eq_ignore_ascii_case(action.as_str()))
    }
}

/// Transcript label for a recorded command: the command line, with the outcome
/// and output on following lines when it did not simply succeed.
pub fn describe_command(command: &psa_api::events::CommandRecord) -> String {
    let mut text = command.command_line();
    match command.outcome {
        psa_api::CommandOutcome::Succeeded => {}
        psa_api::CommandOutcome::Failed => text.push_str("\nFailed"),
        psa_api::CommandOutcome::Interrupted => text.push_str("\nInterrupted"),
    }
    if let Some(output) = &command.output {
        text = format!("{text}\n{output}");
    }
    text
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerInput {
    Text(String),
    Command { id: CommandId, arguments: String },
}
/// A doubled leading slash explicitly sends literal slash text.
pub fn parse_composer_input(input: &str) -> ComposerInput {
    if let Some(literal) = input.strip_prefix("//") {
        return ComposerInput::Text(format!("/{literal}"));
    }
    let Some(command) = input.strip_prefix('/') else {
        return ComposerInput::Text(input.to_owned());
    };
    let (name, arguments) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    match COMMANDS.iter().find(|c| c.name == name) {
        Some(spec) => ComposerInput::Command {
            id: spec.id,
            arguments: arguments.trim().to_owned(),
        },
        None => ComposerInput::Text(input.to_owned()),
    }
}
pub fn complete_commands(input: &str) -> Vec<CommandSpec> {
    let Some(prefix) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.contains(char::is_whitespace) || prefix.starts_with('/') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .copied()
        .collect()
}
#[derive(Debug, Clone, Default)]
pub struct CommandSelection {
    pub selected: usize,
    pub dismissed: bool,
}
impl CommandSelection {
    pub fn next(&mut self, count: usize) {
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }
    pub fn previous(&mut self, count: usize) {
        if count > 0 {
            self.selected = (self.selected + count - 1) % count;
        }
    }
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn accept(&self, candidates: &[CommandSpec]) -> Option<CommandSpec> {
        if self.dismissed {
            None
        } else {
            candidates.get(self.selected).copied()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_command_parses_actions() {
        assert_eq!(
            parse_composer_input("/sandbox stop"),
            ComposerInput::Command {
                id: CommandId::Sandbox,
                arguments: "stop".into()
            }
        );
        assert_eq!(SandboxAction::parse("stop"), Some(SandboxAction::Stop));
        assert_eq!(SandboxAction::parse(" Start "), Some(SandboxAction::Start));
        assert_eq!(SandboxAction::parse(""), None);
        assert_eq!(SandboxAction::parse("restart"), None);
        assert!(CommandId::Sandbox.is_available(true));
        assert!(
            complete_commands("/sand")
                .iter()
                .any(|c| c.id == CommandId::Sandbox)
        );
    }
    #[test]
    fn commands_are_not_model_prompts_and_literals_are_preserved() {
        assert_eq!(
            parse_composer_input("/model provider/model"),
            ComposerInput::Command {
                id: CommandId::SessionModel,
                arguments: "provider/model".into()
            }
        );
        assert_eq!(
            parse_composer_input("//model"),
            ComposerInput::Text("/model".into())
        );
        assert_eq!(
            parse_composer_input("/unknown"),
            ComposerInput::Text("/unknown".into())
        );
        assert_eq!(complete_commands("/mo").len(), 1);
        assert!(complete_commands("//mo").is_empty());
    }
    #[test]
    fn agent_command_parsing_and_completion() {
        assert_eq!(
            parse_composer_input("/agent  worker  "),
            ComposerInput::Command {
                id: CommandId::Agent,
                arguments: "worker".into(),
            }
        );
        assert_eq!(
            parse_composer_input("/agent"),
            ComposerInput::Command {
                id: CommandId::Agent,
                arguments: String::new(),
            }
        );
        assert_eq!(
            parse_composer_input("//agent"),
            ComposerInput::Text("/agent".into())
        );
        assert_eq!(
            parse_composer_input("/agents"),
            ComposerInput::Text("/agents".into())
        );
        assert_eq!(complete_commands("/ag")[0].id, CommandId::Agent);
        assert!(complete_commands("/agent worker").is_empty());
    }
    #[test]
    fn effort_command_parses_and_completes() {
        assert_eq!(
            parse_composer_input("/effort high"),
            ComposerInput::Command {
                id: CommandId::SessionEffort,
                arguments: "high".into(),
            }
        );
        assert_eq!(complete_commands("/eff")[0].id, CommandId::SessionEffort);
        assert_eq!(
            parse_composer_input("//effort high"),
            ComposerInput::Text("/effort high".into())
        );
    }
    #[test]
    fn compact_and_rename_parse_with_arguments_and_respect_availability() {
        assert_eq!(
            parse_composer_input("/compact focus on tests"),
            ComposerInput::Command {
                id: CommandId::Compact,
                arguments: "focus on tests".into(),
            }
        );
        assert_eq!(
            parse_composer_input("/rename"),
            ComposerInput::Command {
                id: CommandId::Rename,
                arguments: String::new(),
            }
        );
        assert!(!CommandId::Compact.is_available(true));
        assert!(CommandId::Compact.is_available(false));
        assert!(CommandId::Rename.is_available(true) && CommandId::Rename.is_available(false));
        assert!(!CommandId::Agent.is_available(false));
        assert!(CommandId::Refresh.is_available(true) && CommandId::Refresh.is_available(false));
        assert!(!CommandId::Fork.is_available(true) && CommandId::Fork.is_available(false));
        assert_eq!(
            parse_composer_input("/fork try another approach"),
            ComposerInput::Command {
                id: CommandId::Fork,
                arguments: "try another approach".into(),
            }
        );
        let record = psa_api::events::CommandRecord {
            name: "compact".into(),
            args: "tests".into(),
            outcome: psa_api::CommandOutcome::Failed,
            output: Some("no summary".into()),
        };
        assert_eq!(
            describe_command(&record),
            "/compact tests\nFailed\nno summary"
        );
    }
    #[test]
    fn empty_and_dismissed_completion_are_safe() {
        let mut selection = CommandSelection::default();
        selection.next(0);
        selection.previous(0);
        assert_eq!(selection.accept(&[]), None);
        selection.dismiss();
        assert_eq!(selection.accept(COMMANDS), None);
        selection.reset();
        assert!(selection.accept(COMMANDS).is_some());
    }
}
