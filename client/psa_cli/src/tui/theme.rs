//! Colors for each kind of text, chosen by the `theme` setting.
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    /// Colors for a dark terminal background.
    #[default]
    Dark,
    /// Colors for a light terminal background.
    Light,
    /// No colors; emphasis through bold, dim, and reversed text.
    Monochrome,
}

impl Theme {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::Monochrome];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Monochrome => "monochrome",
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            Self::Dark => Palette {
                user: Style::new().fg(Color::Cyan),
                subagent: Style::new().fg(Color::Magenta),
                tool: Style::new().fg(Color::Yellow),
                success: Style::new().fg(Color::Green),
                error: Style::new().fg(Color::Red),
                warning: Style::new().fg(Color::Yellow),
                muted: Style::new().fg(Color::DarkGray),
                border: Style::new().fg(Color::DarkGray),
                status: Style::new().bg(Color::DarkGray).fg(Color::White),
                selected: Style::new().add_modifier(Modifier::REVERSED),
            },
            Self::Light => Palette {
                user: Style::new().fg(Color::Blue),
                subagent: Style::new().fg(Color::Magenta),
                tool: Style::new().fg(Color::Rgb(150, 95, 0)),
                success: Style::new().fg(Color::Rgb(0, 120, 40)),
                error: Style::new().fg(Color::Red),
                warning: Style::new().fg(Color::Rgb(150, 95, 0)),
                muted: Style::new().fg(Color::Rgb(110, 110, 110)),
                border: Style::new().fg(Color::Rgb(170, 170, 170)),
                status: Style::new().bg(Color::Rgb(220, 220, 220)).fg(Color::Black),
                selected: Style::new().add_modifier(Modifier::REVERSED),
            },
            Self::Monochrome => Palette {
                user: Style::new().add_modifier(Modifier::BOLD),
                subagent: Style::new().add_modifier(Modifier::BOLD),
                tool: Style::new(),
                success: Style::new(),
                error: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                warning: Style::new().add_modifier(Modifier::BOLD),
                muted: Style::new().add_modifier(Modifier::DIM),
                border: Style::new(),
                status: Style::new().add_modifier(Modifier::REVERSED),
                selected: Style::new().add_modifier(Modifier::REVERSED),
            },
        }
    }
}

/// Styles for each role of text on screen.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub user: Style,
    pub subagent: Style,
    pub tool: Style,
    pub success: Style,
    pub error: Style,
    pub warning: Style,
    /// Secondary text: hints, tool output, thinking.
    pub muted: Style,
    pub border: Style,
    pub status: Style,
    pub selected: Style,
}
