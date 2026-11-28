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
    /// No colors; emphasis through bold, dim, and underlined text.
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
                // The desktop's muted text color.
                muted: Style::new().fg(Color::Rgb(146, 155, 176)),
                border: Style::new().fg(Color::DarkGray),
                selected: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                effort_low: Style::new().fg(Color::Rgb(155, 174, 201)),
                effort_medium: Style::new().fg(Color::Rgb(213, 180, 123)),
                effort_high: Style::new().fg(Color::Rgb(193, 165, 222)),
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
                selected: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                effort_low: Style::new().fg(Color::Rgb(60, 90, 140)),
                effort_medium: Style::new().fg(Color::Rgb(150, 100, 0)),
                effort_high: Style::new().fg(Color::Rgb(120, 70, 170)),
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
                selected: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                effort_low: Style::new(),
                effort_medium: Style::new(),
                effort_high: Style::new().add_modifier(Modifier::BOLD),
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
    /// The highlighted row of a menu or list: colored and bold, never a
    /// background fill.
    pub selected: Style,
    /// Effort names, colored as in the desktop's status line.
    pub effort_low: Style,
    pub effort_medium: Style,
    pub effort_high: Style,
}

impl Palette {
    pub fn effort(&self, effort: llm_api::providers::Effort) -> Style {
        use llm_api::providers::Effort;
        match effort {
            Effort::Off | Effort::Minimal | Effort::Low => self.effort_low,
            Effort::Medium => self.effort_medium,
            Effort::High | Effort::Xhigh | Effort::Max => self.effort_high,
        }
    }
}
