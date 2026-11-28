//! Client commands are UI actions, never accidentally forwarded as prompt text.
//! One registry serves every client UI: each command says what arguments it
//! takes, where it can run, whether it is saved to the transcript, and which
//! front ends implement it. [`menu`] builds the completion menu from it.
pub mod menu;

pub use menu::*;

/// A client UI that offers slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Gui,
    Tui,
}

const ALL_FRONTENDS: &[Frontend] = &[Frontend::Gui, Frontend::Tui];
const TUI_ONLY: &[Frontend] = &[Frontend::Tui];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    SessionModel,
    SessionEffort,
    SelectAgent,
    Refresh,
    Info,
    Compact,
    Goal,
    Rename,
    Fork,
    Sandbox,
    Sessions,
    NewSession,
    SelectProbe,
    Agents,
    Probes,
    PinnedModels,
    PinnedDecisionModels,
    Server,
    Settings,
    Help,
    Quit,
}

impl CommandId {
    pub fn spec(self) -> &'static CommandSpec {
        COMMANDS
            .iter()
            .find(|spec| spec.id == self)
            .expect("every command is registered")
    }

    pub fn is_available(self, context: CommandContext) -> bool {
        self.spec().scope.allows(context)
    }
}

/// Where a composer is when a command is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandContext {
    /// No session is open.
    NoSession,
    /// A new session that exists only in the client until its first message,
    /// as the TUI's `/new` makes.
    SessionDraft,
    /// A created session whose root agent has not been created yet, as the
    /// desktop's new sessions are.
    DraftRoot,
    /// An existing agent.
    Agent,
}

/// Where a command can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Anywhere, even without an open session.
    Global,
    /// In any composer, including a session that is not created yet.
    Composer,
    /// In a session that exists on the server.
    Session,
    /// Only on an existing agent.
    Agent,
    /// Before the root agent exists, whether or not its session does.
    RootDraft,
    /// Only in a session that is not created yet.
    SessionDraft,
}

impl CommandScope {
    pub fn allows(self, context: CommandContext) -> bool {
        use CommandContext::*;
        match self {
            Self::Global => true,
            Self::Composer => context != NoSession,
            Self::Session => matches!(context, DraftRoot | Agent),
            Self::Agent => context == Agent,
            Self::RootDraft => matches!(context, DraftRoot | SessionDraft),
            Self::SessionDraft => context == SessionDraft,
        }
    }
}

/// Whether a command is saved to the agent's transcript, and by whom. Commands
/// that change the agent or its session are saved; ones that only show
/// something are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRecord {
    /// Never saved.
    No,
    /// The client saves it once the command ends.
    Client,
    /// The server saves it as part of running it, such as `/compact`.
    Server,
}

/// What a command's arguments are, which also decides its completion menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arguments {
    None,
    /// Free text, described by its hint, such as `name`.
    Text(&'static str),
    Subcommands(SubcommandSet),
    /// A model from the catalog, searched by what is typed.
    Models,
    /// An effort the session's current model supports.
    Efforts,
    /// A root agent configuration.
    Configurations,
    /// A connected probe to start the session on.
    Probes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubcommandSet {
    pub options: &'static [Subcommand],
    /// What the command does with no arguments; offered first in the menu.
    pub bare: Option<&'static str>,
    /// Other text is accepted as this argument, such as a new goal.
    pub text: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subcommand {
    pub name: &'static str,
    pub description: &'static str,
    /// Optional text after the subcommand, such as a pause reason.
    pub text: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: CommandId,
    pub name: &'static str,
    pub description: &'static str,
    pub arguments: Arguments,
    pub scope: CommandScope,
    pub transcript: TranscriptRecord,
    /// Client UIs that implement this command.
    pub frontends: &'static [Frontend],
}

impl CommandSpec {
    pub fn supports(&self, frontend: Frontend) -> bool {
        self.frontends.contains(&frontend)
    }

    /// Whether choosing the command opens a menu of its arguments rather
    /// than running it.
    pub fn has_argument_menu(&self) -> bool {
        matches!(
            self.arguments,
            Arguments::Subcommands(_)
                | Arguments::Models
                | Arguments::Efforts
                | Arguments::Configurations
                | Arguments::Probes
        )
    }

    /// The composer text Tab completes the command name to; a trailing space
    /// leaves room for arguments.
    pub fn completion(&self) -> String {
        match self.arguments {
            Arguments::None => format!("/{}", self.name),
            _ => format!("/{} ", self.name),
        }
    }

    /// The command line with its arguments, as help lists it.
    pub fn usage(&self) -> String {
        let name = self.name;
        match self.arguments {
            Arguments::None => format!("/{name}"),
            Arguments::Text(hint) => format!("/{name} [{hint}]"),
            Arguments::Models => format!("/{name} [search]"),
            Arguments::Efforts => format!("/{name} [effort]"),
            Arguments::Configurations => format!("/{name} [configuration]"),
            Arguments::Probes => format!("/{name} [probe]"),
            Arguments::Subcommands(set) => {
                let mut choices: Vec<String> = set
                    .options
                    .iter()
                    .map(|option| match option.text {
                        Some(hint) => format!("{} [{hint}]", option.name),
                        None => option.name.to_owned(),
                    })
                    .collect();
                if let Some(text) = set.text {
                    choices.insert(0, text.to_owned());
                }
                let choices = choices.join(" | ");
                if set.bare.is_some() {
                    format!("/{name} [{choices}]")
                } else {
                    format!("/{name} {choices}")
                }
            }
        }
    }
}

const GOAL_SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "pause",
        description: "Pause the goal; optional reason",
        text: Some("reason"),
    },
    Subcommand {
        name: "resume",
        description: "Resume a paused goal",
        text: None,
    },
    Subcommand {
        name: "clear",
        description: "Drop the goal without completing it; optional reason",
        text: Some("reason"),
    },
];

const SANDBOX_SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "start",
        description: "Start this session's sandbox container",
        text: None,
    },
    Subcommand {
        name: "stop",
        description: "Stop this session's sandbox container",
        text: None,
    },
];

const SERVER_SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "status",
        description: "Show the local server's address and log",
        text: None,
    },
    Subcommand {
        name: "restart",
        description: "Restart the local server and reconnect",
        text: None,
    },
    Subcommand {
        name: "stop",
        description: "Stop the local server",
        text: None,
    },
];

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::SessionModel,
        name: "model",
        description: "Change this session's default model",
        arguments: Arguments::Models,
        scope: CommandScope::Composer,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::SelectAgent,
        name: "select agent",
        description: "Choose the root agent and its model policy",
        arguments: Arguments::Configurations,
        scope: CommandScope::RootDraft,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Refresh,
        name: "refresh",
        description: "Reload agent and skill configurations from disk",
        arguments: Arguments::None,
        scope: CommandScope::Composer,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::SessionEffort,
        name: "effort",
        description: "Change this session's default reasoning effort",
        arguments: Arguments::Efforts,
        scope: CommandScope::Composer,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Info,
        name: "info",
        description: "Show agent information and token usage",
        arguments: Arguments::None,
        scope: CommandScope::Composer,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Compact,
        name: "compact",
        description: "Summarize this agent's older context now; optional focus instructions",
        arguments: Arguments::Text("focus"),
        scope: CommandScope::Agent,
        transcript: TranscriptRecord::Server,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Goal,
        name: "goal",
        description: "Show or set this agent's goal; pause, resume, or clear it",
        arguments: Arguments::Subcommands(SubcommandSet {
            options: GOAL_SUBCOMMANDS,
            bare: Some("Show the current goal"),
            text: Some("new goal"),
        }),
        scope: CommandScope::Agent,
        transcript: TranscriptRecord::Server,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Rename,
        name: "rename",
        description: "Rename this session",
        arguments: Arguments::Text("name"),
        scope: CommandScope::Composer,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Fork,
        name: "fork",
        description: "Fork this session at its current point; optional new name",
        arguments: Arguments::Text("name"),
        scope: CommandScope::Agent,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Sandbox,
        name: "sandbox",
        description: "Start or stop this session's sandbox",
        arguments: Arguments::Subcommands(SubcommandSet {
            options: SANDBOX_SUBCOMMANDS,
            bare: None,
            text: None,
        }),
        scope: CommandScope::Session,
        transcript: TranscriptRecord::Client,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Sessions,
        name: "sessions",
        description: "Show recent sessions",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::NewSession,
        name: "new",
        description: "Start a new session in this session's directory",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::SelectProbe,
        name: "select probe",
        description: "Choose the probe a new session starts on",
        arguments: Arguments::Probes,
        scope: CommandScope::SessionDraft,
        transcript: TranscriptRecord::No,
        frontends: TUI_ONLY,
    },
    CommandSpec {
        id: CommandId::Agents,
        name: "agents",
        description: "Show this session and its agents",
        arguments: Arguments::None,
        scope: CommandScope::Session,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Probes,
        name: "probes",
        description: "Show connected probes and their sandboxes",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::PinnedModels,
        name: "pinned-models",
        description: "Pin or unpin models; /model lists the pinned ones",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::PinnedDecisionModels,
        name: "pinned-decision-models",
        description: "Pin or unpin decision models",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Server,
        name: "server",
        description: "Show, restart, or stop the local server",
        arguments: Arguments::Subcommands(SubcommandSet {
            options: SERVER_SUBCOMMANDS,
            bare: None,
            text: None,
        }),
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Settings,
        name: "settings",
        description: "Open settings",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Help,
        name: "help",
        description: "List commands and keys",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
    },
    CommandSpec {
        id: CommandId::Quit,
        name: "quit",
        description: "Exit",
        arguments: Arguments::None,
        scope: CommandScope::Global,
        transcript: TranscriptRecord::No,
        frontends: ALL_FRONTENDS,
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

    pub fn change(self) -> SandboxChange {
        match self {
            Self::Start => SandboxChange::Start,
            Self::Stop => SandboxChange::Stop,
        }
    }

    /// Parse `/sandbox` arguments; `None` for empty or unrecognized actions.
    pub fn parse(arguments: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| arguments.trim().eq_ignore_ascii_case(action.as_str()))
    }
}

/// `/server` subcommands for the local server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAction {
    Status,
    Restart,
    Stop,
}

impl ServerAction {
    pub const ALL: [Self; 3] = [Self::Status, Self::Restart, Self::Stop];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Restart => "restart",
            Self::Stop => "stop",
        }
    }

    pub fn parse(arguments: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str().eq_ignore_ascii_case(arguments.trim()))
    }
}

/// The transcript record for a command the client saves, or `None` when the
/// command is not saved by the client.
pub fn build_command_record(
    id: CommandId,
    arguments: &str,
    ended: Result<(), String>,
) -> Option<psa::api::events::CommandRecord> {
    let spec = id.spec();
    if spec.transcript != TranscriptRecord::Client {
        return None;
    }
    let (outcome, output) = match ended {
        Ok(()) => (psa::api::CommandOutcome::Succeeded, None),
        Err(message) => (psa::api::CommandOutcome::Failed, Some(message)),
    };
    Some(psa::api::events::CommandRecord {
        name: spec.name.into(),
        args: arguments.trim().into(),
        outcome,
        output,
    })
}

/// Transcript label for a recorded command: the command line, with the outcome
/// and output on following lines when it did not simply succeed.
pub fn describe_command(command: &psa::api::events::CommandRecord) -> String {
    let mut text = command.command_line();
    match command.outcome {
        psa::api::CommandOutcome::Succeeded => {}
        psa::api::CommandOutcome::Failed => text.push_str("\nFailed"),
        psa::api::CommandOutcome::Interrupted => text.push_str("\nInterrupted"),
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

/// A doubled leading slash explicitly sends literal slash text. Commands the
/// front end does not implement are sent as text, like unknown commands.
pub fn parse_composer_input(input: &str, frontend: Frontend) -> ComposerInput {
    if let Some(literal) = input.strip_prefix("//") {
        return ComposerInput::Text(format!("/{literal}"));
    }
    let Some(command) = input.strip_prefix('/') else {
        return ComposerInput::Text(input.to_owned());
    };
    match split_command_line(command, frontend) {
        Some((spec, arguments)) => ComposerInput::Command {
            id: spec.id,
            arguments: arguments.trim().to_owned(),
        },
        None => ComposerInput::Text(input.to_owned()),
    }
}

/// The command `line` (without its slash) starts with, and the text after
/// its name. Names can be several words, such as `select agent`; the longest
/// matching name wins.
pub fn split_command_line(line: &str, frontend: Frontend) -> Option<(&'static CommandSpec, &str)> {
    COMMANDS
        .iter()
        .filter(|spec| spec.supports(frontend))
        .filter_map(|spec| {
            let rest = line.strip_prefix(spec.name)?;
            let ends_name = rest.is_empty() || rest.starts_with(char::is_whitespace);
            ends_name.then_some((spec, rest))
        })
        .max_by_key(|(spec, _)| spec.name.len())
}

pub fn find_command(name: &str, frontend: Frontend) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|c| c.name == name && c.supports(frontend))
}

/// Commands whose name starts with the typed `/prefix`, an exact match first.
/// The prefix can reach into a several-word name, as `/select a` does.
pub fn complete_commands(input: &str, frontend: Frontend) -> Vec<CommandSpec> {
    let Some(prefix) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.starts_with('/') {
        return Vec::new();
    }
    let mut commands: Vec<CommandSpec> = COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix) && c.supports(frontend))
        .copied()
        .collect();
    commands.sort_by_key(|command| command.name != prefix);
    commands
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

    /// The highlighted index, kept within `count` candidates.
    pub fn index(&self, count: usize) -> usize {
        self.selected.min(count.saturating_sub(1))
    }
}

#[cfg(test)]
#[path = "../tests/commands/tests.rs"]
mod tests;
