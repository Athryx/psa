//! TUI settings in `tui.toml` in the data directory. They use the desktop
//! client's preference names and options where both clients have a setting, but
//! live in their own file: each client rewrites its whole file, so sharing one
//! would let either client overwrite the other's changes.
use super::theme::Theme;
use client_shared::{ReasoningDisplay, TranscriptLogLevel, TurnDividerDisplay};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Tool output lines the settings dialog offers; 0 hides output.
const TOOL_OUTPUT_CHOICES: [u16; 5] = [0, 3, 10, 30, 100];
/// Transcript widths the settings dialog offers; 0 uses the terminal's width.
const MAX_WIDTH_CHOICES: [u16; 5] = [0, 80, 100, 120, 160];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiSettings {
    pub preferences: TuiPreferences,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiPreferences {
    pub theme: Theme,
    pub reasoning_display: ReasoningDisplay,
    pub turn_dividers: TurnDividerDisplay,
    pub log_level: TranscriptLogLevel,
    /// Harness messages, such as goal reminders, sent to the model.
    pub show_harness_messages: bool,
    pub hide_text_recorded_results: bool,
    /// Hide running and idle changes; interruptions are always shown.
    pub hide_agent_statuses: bool,
    pub show_hidden_agents: bool,
    /// Tool output lines shown under each call; 0 hides output.
    pub tool_output_lines: u16,
    /// Transcript wrap width in columns; 0 uses the terminal's width.
    pub max_width: u16,
}

impl Default for TuiPreferences {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            reasoning_display: ReasoningDisplay::Collapsed,
            turn_dividers: TurnDividerDisplay::Labeled,
            log_level: TranscriptLogLevel::Error,
            show_harness_messages: true,
            hide_text_recorded_results: true,
            hide_agent_statuses: true,
            show_hidden_agents: false,
            tool_output_lines: 3,
            max_width: 0,
        }
    }
}

impl TuiPreferences {
    /// Everything shown, for the Ctrl+O details view.
    pub fn with_details(&self) -> Self {
        Self {
            reasoning_display: ReasoningDisplay::Shown,
            log_level: TranscriptLogLevel::Info,
            show_harness_messages: true,
            hide_text_recorded_results: false,
            hide_agent_statuses: false,
            tool_output_lines: self.tool_output_lines.max(60),
            ..self.clone()
        }
    }
}

/// One line of the settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingRow {
    Theme,
    Reasoning,
    TurnDividers,
    Logs,
    HarnessMessages,
    TextResults,
    AgentStatuses,
    HiddenAgents,
    ToolOutputLines,
    MaxWidth,
}

impl SettingRow {
    pub const ALL: [Self; 10] = [
        Self::Theme,
        Self::Reasoning,
        Self::TurnDividers,
        Self::Logs,
        Self::HarnessMessages,
        Self::TextResults,
        Self::AgentStatuses,
        Self::HiddenAgents,
        Self::ToolOutputLines,
        Self::MaxWidth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Reasoning => "Reasoning",
            Self::TurnDividers => "Dividers between model turns",
            Self::Logs => "Harness logs",
            Self::HarnessMessages => "Harness messages (goal reminders, nudges)",
            Self::TextResults => "Text recorded results",
            Self::AgentStatuses => "Agent statuses (interrupted always shown)",
            Self::HiddenAgents => "Internal agents in /agents",
            Self::ToolOutputLines => "Tool output lines",
            Self::MaxWidth => "Transcript width",
        }
    }

    pub fn value(self, preferences: &TuiPreferences) -> String {
        match self {
            Self::Theme => preferences.theme.label().into(),
            Self::Reasoning => match preferences.reasoning_display {
                ReasoningDisplay::Hidden => "hidden",
                ReasoningDisplay::Collapsed => "first line",
                ReasoningDisplay::Shown => "shown",
            }
            .into(),
            Self::TurnDividers => match preferences.turn_dividers {
                TurnDividerDisplay::Hidden => "hidden",
                TurnDividerDisplay::Line => "line",
                TurnDividerDisplay::Labeled => "line with labels",
            }
            .into(),
            Self::Logs => match preferences.log_level {
                TranscriptLogLevel::Hidden => "none",
                TranscriptLogLevel::Error => "errors only",
                TranscriptLogLevel::Info => "info and errors",
            }
            .into(),
            Self::HarnessMessages => describe_shown(preferences.show_harness_messages),
            Self::TextResults => describe_shown(!preferences.hide_text_recorded_results),
            Self::AgentStatuses => describe_shown(!preferences.hide_agent_statuses),
            Self::HiddenAgents => describe_shown(preferences.show_hidden_agents),
            Self::ToolOutputLines => match preferences.tool_output_lines {
                0 => "hidden".into(),
                lines => lines.to_string(),
            },
            Self::MaxWidth => match preferences.max_width {
                0 => "terminal width".into(),
                width => format!("{width} columns"),
            },
        }
    }

    /// Move to the next option, or the previous one when `forward` is false.
    pub fn cycle(self, preferences: &mut TuiPreferences, forward: bool) {
        match self {
            Self::Theme => preferences.theme = step(&Theme::ALL, preferences.theme, forward),
            Self::Reasoning => {
                preferences.reasoning_display = step(
                    &[
                        ReasoningDisplay::Hidden,
                        ReasoningDisplay::Collapsed,
                        ReasoningDisplay::Shown,
                    ],
                    preferences.reasoning_display,
                    forward,
                )
            }
            Self::TurnDividers => {
                preferences.turn_dividers = step(
                    &[
                        TurnDividerDisplay::Hidden,
                        TurnDividerDisplay::Line,
                        TurnDividerDisplay::Labeled,
                    ],
                    preferences.turn_dividers,
                    forward,
                )
            }
            Self::Logs => {
                preferences.log_level = step(
                    &[
                        TranscriptLogLevel::Hidden,
                        TranscriptLogLevel::Error,
                        TranscriptLogLevel::Info,
                    ],
                    preferences.log_level,
                    forward,
                )
            }
            Self::HarnessMessages => toggle(&mut preferences.show_harness_messages),
            Self::TextResults => toggle(&mut preferences.hide_text_recorded_results),
            Self::AgentStatuses => toggle(&mut preferences.hide_agent_statuses),
            Self::HiddenAgents => toggle(&mut preferences.show_hidden_agents),
            Self::ToolOutputLines => {
                preferences.tool_output_lines =
                    step(&TOOL_OUTPUT_CHOICES, preferences.tool_output_lines, forward)
            }
            Self::MaxWidth => {
                preferences.max_width = step(&MAX_WIDTH_CHOICES, preferences.max_width, forward)
            }
        }
    }
}

fn describe_shown(shown: bool) -> String {
    if shown { "shown" } else { "hidden" }.into()
}

fn toggle(value: &mut bool) {
    *value = !*value;
}

/// The option after (or before) `current`; a value not in `options`, such as a
/// hand-edited number, moves to the first option.
fn step<T: Copy + PartialEq>(options: &[T], current: T, forward: bool) -> T {
    let Some(index) = options.iter().position(|option| *option == current) else {
        return options[0];
    };
    let count = options.len();
    let next = if forward {
        (index + 1) % count
    } else {
        (index + count - 1) % count
    };
    options[next]
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot access TUI settings at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TUI settings at {0}; fix or remove the file")]
    Invalid(PathBuf),
    #[error("cannot serialize TUI settings")]
    Serialize,
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(psa_config::paths::TUI_SETTINGS_FILE)
}

/// A missing file gives the defaults. A malformed one is an error rather than
/// being replaced, so hand edits are never lost.
pub async fn load_settings(path: &Path) -> Result<TuiSettings, SettingsError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TuiSettings::default());
        }
        Err(source) => {
            return Err(SettingsError::Io {
                path: path.into(),
                source,
            });
        }
    };
    toml::from_str(&text).map_err(|_| SettingsError::Invalid(path.into()))
}

/// Replace the file atomically through a private temporary file beside it.
pub async fn save_settings(path: &Path, settings: &TuiSettings) -> Result<(), SettingsError> {
    let contents = toml::to_string_pretty(settings).map_err(|_| SettingsError::Serialize)?;
    let io_error = |source| SettingsError::Io {
        path: path.into(),
        source,
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent).await.map_err(io_error)?;

    let temporary = parent.join(format!(".tui-{}.toml.tmp", std::process::id()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut file = options.open(&temporary).await?;
        file.write_all(contents.as_bytes()).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temporary, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result.map_err(io_error)
}

#[cfg(test)]
#[path = "../tests/tui/settings/tests.rs"]
mod tests;
