//! Draws the app: transcript, message line, input, status bar, and dialogs.
use super::{
    app::{App, Overlay, PromptDialog, QuestionDialog, SettingsTab, StatusMessage},
    commands::describe_help,
    picker::Picker,
    settings::{SettingRow, TuiPreferences},
    text::{truncate_line, wrap_text},
    theme::Palette,
    transcript_view::{DisplayOptions, build_lines},
};
use client_shared::CandidateKind;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

const MAX_INPUT_LINES: usize = 8;
const MAX_COMPLETIONS: usize = 8;
const PROMPT: &str = "› ";
/// Replaces the prompt while the agent works: an eight-dot braille spinner.
const SPINNER: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let palette = app.palette();
    // Borders and the prompt take four columns.
    let input_width = area.width.saturating_sub(4);
    let (input_lines, cursor) = app.composer.layout(input_width.max(1));
    let input_height = input_lines.len().clamp(1, MAX_INPUT_LINES) as u16 + 2;
    // The message line takes room only while it has something to say.
    let message_height = u16::from(message_line(app, &palette, 1).is_some());
    let [transcript, message, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(message_height),
        Constraint::Length(input_height),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_transcript(frame, app, &palette, transcript);
    draw_message(frame, app, &palette, message);
    draw_input(frame, app, &palette, input, &input_lines, cursor);
    super::status_bar::draw_status(frame, app, &palette, status);
    draw_completions(frame, app, &palette, input);
    draw_overlay(frame, app, &palette, area);
}

fn draw_transcript(frame: &mut Frame, app: &mut App, palette: &Palette, area: Rect) {
    // The last column is kept for the scrollbar.
    let [area, bar] = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let preferences = app.display_preferences();
    let width = match preferences.max_width {
        0 => area.width,
        max => area.width.min(max),
    };
    let lines = match &app.view {
        Some(view) => {
            let options = DisplayOptions {
                preferences: &preferences,
                palette,
                details: app.details,
            };
            build_lines(&view.transcript, &options, width)
        }
        None if app.draft.is_some() => draft_lines(app, palette),
        None => welcome_lines(),
    };
    let height = usize::from(area.height);
    let total = lines.len();

    // Keep a scrolled-up view on the same lines as new output arrives below.
    if app.scroll > 0 && total > app.last_line_count {
        app.scroll += total - app.last_line_count;
    }
    app.last_line_count = total;
    app.page_height = height;
    app.scroll = app.scroll.min(total.saturating_sub(height));

    let end = total - app.scroll;
    let start = end.saturating_sub(height);
    let visible: Vec<Line> = lines[start..end].to_vec();
    frame.render_widget(Paragraph::new(visible), area);
    draw_scrollbar(frame, app, palette, bar, start);

    if app.scroll > 0 {
        let label = format!(" ↓ {} more lines (Ctrl+End) ", app.scroll);
        let width = (label.chars().count() as u16).min(area.width);
        let corner = Rect {
            x: area.right().saturating_sub(width),
            y: area.bottom().saturating_sub(1),
            width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(label).style(palette.selected), corner);
    }
}

/// The transcript's scrollbar, when it is longer than the window; `start` is
/// the first line shown.
fn draw_scrollbar(frame: &mut Frame, app: &mut App, palette: &Palette, bar: Rect, start: usize) {
    let max_start = app.last_line_count.saturating_sub(app.page_height);
    if max_start == 0 || bar.width == 0 {
        app.scrollbar = None;
        return;
    }
    app.scrollbar = Some(bar);
    let mut state = ScrollbarState::new(max_start + 1)
        .position(start)
        .viewport_content_length(app.page_height);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(palette.border)
        .thumb_symbol("┃");
    frame.render_stateful_widget(scrollbar, bar, &mut state);
}

/// A draft's window: what the first message will create, and how to change it.
fn draft_lines(app: &App, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled("New session", Style::new().add_modifier(Modifier::BOLD)),
        Line::default(),
    ];
    lines.extend(app.describe_draft().into_iter().map(Line::raw));
    lines.push(Line::default());
    let pending = app.draft.as_ref().is_some_and(|draft| draft.pending);
    let hint = if pending {
        "Starting the session…"
    } else {
        "The first message creates the session. /rename names it; /select agent, /select probe and /model change it."
    };
    lines.push(Line::styled(hint, palette.muted));
    lines
}

fn welcome_lines() -> Vec<Line<'static>> {
    vec![
        Line::styled("PSA", Style::new().add_modifier(Modifier::BOLD)),
        Line::default(),
        Line::raw("No session is open."),
        Line::raw("  /sessions   open a recent session"),
        Line::raw("  /new        start a session in the current directory"),
        Line::raw("  /settings   change the theme and what transcripts show"),
        Line::raw("  /help       list commands and keys"),
    ]
}

fn draw_message(frame: &mut Frame, app: &App, palette: &Palette, area: Rect) {
    if let Some(line) = message_line(app, palette, usize::from(area.width)) {
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// A notice, an error, or a reminder of a waiting question.
fn message_line(app: &App, palette: &Palette, width: usize) -> Option<Line<'static>> {
    let waiting = app
        .view
        .as_ref()
        .and_then(|view| view.pending_query())
        .is_some_and(|query| app.dismissed_queries.contains(&query.id));
    Some(match &app.message {
        Some(StatusMessage::Error(text)) => Line::styled(truncate_line(text, width), palette.error),
        Some(StatusMessage::Notice(text)) => {
            Line::styled(truncate_line(text, width), palette.success)
        }
        None if waiting => Line::styled(
            truncate_line("The agent is waiting for an answer: Ctrl+Q", width),
            palette.warning,
        ),
        None => return None,
    })
}

fn draw_input(
    frame: &mut Frame,
    app: &App,
    palette: &Palette,
    area: Rect,
    lines: &[String],
    cursor: (u16, u16),
) {
    let first = usize::from(cursor.1).saturating_sub(MAX_INPUT_LINES - 1);
    let prompt = match app.is_animating() {
        // The screen redraws every third tick while animating.
        true => format!("{} ", SPINNER[(app.tick / 3) as usize % SPINNER.len()]),
        false => PROMPT.to_owned(),
    };
    let shown: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(MAX_INPUT_LINES)
        .map(|(index, line)| {
            let prompt = if index == 0 {
                prompt.clone()
            } else {
                "  ".into()
            };
            Line::from(vec![
                Span::styled(prompt, palette.user),
                Span::raw(line.clone()),
            ])
        })
        .collect();
    let block = Block::bordered().border_style(palette.border);
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(shown).block(block), area);

    if app.overlay.is_none() {
        frame.set_cursor_position(Position {
            x: inner.x + PROMPT.chars().count() as u16 + cursor.0,
            y: inner.y + cursor.1 - first as u16,
        });
    }
}

fn draw_completions(frame: &mut Frame, app: &App, palette: &Palette, input: Rect) {
    if app.overlay.is_some() {
        return;
    }
    let Some(menu) = app.command_menu() else {
        return;
    };
    let width = input.width.min(80);
    let text_width = usize::from(width.saturating_sub(2));

    let mut lines = Vec::new();
    if let Some(note) = &menu.note {
        lines.push(Line::styled(
            truncate_line(&format!(" {note}"), text_width),
            palette.muted,
        ));
    }
    if !menu.has_choices() {
        lines.push(Line::styled(format!(" {}", menu.empty_hint), palette.muted));
    }
    let count = menu.candidates.len();
    let selected = app.completion.index(count);
    let first = selected.saturating_sub(MAX_COMPLETIONS - 1);
    let label_width = menu
        .candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::Choice)
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(32);
    for (index, candidate) in menu
        .candidates
        .iter()
        .enumerate()
        .skip(first)
        .take(MAX_COMPLETIONS)
    {
        let text = match candidate.kind {
            CandidateKind::Choice => {
                format!(" {:<label_width$}  {}", candidate.label, candidate.detail)
            }
            CandidateKind::Toggle => format!(" ── {} ──", candidate.label),
        };
        let line = truncate_line(&text, text_width);
        lines.push(if index == selected {
            Line::styled(line, palette.selected)
        } else if candidate.kind == CandidateKind::Toggle {
            Line::styled(line, palette.muted)
        } else {
            Line::raw(line)
        });
    }

    let height = (lines.len() as u16 + 2).min(input.y);
    let area = Rect {
        x: input.x,
        y: input.y.saturating_sub(height),
        width,
        height,
    };
    let hint = if menu.candidates.iter().any(|c| c.completion.is_some()) {
        " ↑↓ select · Tab complete · Enter choose "
    } else {
        " ↑↓ select · Enter choose "
    };
    let block = Block::bordered()
        .title(format!(" {} ", menu.title))
        .title_bottom(hint)
        .border_style(palette.border);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_overlay(frame: &mut Frame, app: &App, palette: &Palette, area: Rect) {
    match &app.overlay {
        None => {}
        Some(Overlay::Picker(picker)) => draw_picker(frame, picker, palette, area),
        Some(Overlay::Panel {
            title,
            lines,
            scroll,
        }) => draw_panel(frame, title, lines, *scroll, area),
        Some(Overlay::Question(dialog)) => draw_question(frame, dialog, palette, area),
        Some(Overlay::Prompt(dialog)) => draw_prompt(frame, dialog, palette, area),
        Some(Overlay::Settings {
            tab: SettingsTab::Appearance,
            selected,
            ..
        }) => draw_settings(frame, &app.settings.preferences, *selected, palette, area),
        Some(Overlay::Settings {
            tab: SettingsTab::Commands,
            selected,
            ..
        }) => draw_commands_tab(frame, *selected, palette, area),
    }
}

fn dialog_area(area: Rect, height: u16) -> Rect {
    let width = area.width.saturating_sub(4).min(100);
    let height = height
        .min(area.height.saturating_sub(2))
        .max(3)
        .min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    }
}

fn draw_picker(frame: &mut Frame, picker: &Picker, palette: &Palette, area: Rect) {
    let dialog = dialog_area(area, area.height * 2 / 3);
    let block = Block::bordered()
        .title(format!(" {} ", picker.title))
        .padding(Padding::horizontal(1))
        .title_bottom(format!(
            " ↑↓ select · Enter {} · Esc close ",
            picker.enter_hint
        ));
    let inner = block.inner(dialog);
    frame.render_widget(Clear, dialog);
    frame.render_widget(block, dialog);

    let width = usize::from(inner.width);
    let mut lines: Vec<Line> = picker
        .header
        .iter()
        .map(|line| Line::raw(truncate_line(line, width)))
        .collect();
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.extend([
        Line::from(vec![
            Span::styled("Filter: ", palette.muted),
            Span::raw(picker.filter.clone()),
        ]),
        Line::default(),
    ]);
    let visible = picker.visible();
    if picker.loading {
        lines.push(Line::styled("Loading…", palette.muted));
    } else if visible.is_empty() {
        lines.push(Line::styled("No matches", palette.muted));
    }

    let rows = usize::from(inner.height).saturating_sub(lines.len()).max(1);
    let selected = picker.selected_index();
    let first = selected.saturating_sub(rows - 1);
    for (index, item) in visible.iter().enumerate().skip(first).take(rows) {
        let text = format!("{}  {}", item.label, item.detail);
        let text = truncate_line(&text, width);
        let (label_style, detail_style) = if index == selected {
            (palette.selected, palette.selected)
        } else {
            (Style::new(), palette.muted)
        };
        let label_width = item.label.chars().count().min(text.chars().count());
        let (label, detail): (String, String) = (
            text.chars().take(label_width).collect(),
            text.chars().skip(label_width).collect(),
        );
        lines.push(Line::from(vec![
            Span::styled(label, label_style.add_modifier(Modifier::BOLD)),
            Span::styled(detail, detail_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_panel(frame: &mut Frame, title: &str, lines: &[String], scroll: u16, area: Rect) {
    let width = usize::from(area.width.saturating_sub(10).min(94));
    let wrapped: Vec<Line> = lines
        .iter()
        .flat_map(|line| wrap_text(line, width))
        .map(Line::raw)
        .collect();
    let dialog = dialog_area(area, wrapped.len() as u16 + 2);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .padding(Padding::horizontal(1))
        .title_bottom(" Esc close ");
    let visible = usize::from(dialog.height.saturating_sub(2));
    let scroll = scroll.min(wrapped.len().saturating_sub(visible) as u16);
    frame.render_widget(Clear, dialog);
    frame.render_widget(
        Paragraph::new(wrapped).block(block).scroll((scroll, 0)),
        dialog,
    );
}

fn draw_settings(
    frame: &mut Frame,
    preferences: &TuiPreferences,
    selected: usize,
    palette: &Palette,
    area: Rect,
) {
    let label_width = SettingRow::ALL
        .iter()
        .map(|row| row.label().chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = SettingRow::ALL
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let label = format!("{:<label_width$}  ", row.label());
            let value = format!("‹ {} ›", row.value(preferences));
            let (label_style, value_style) = if index == selected {
                (palette.selected, palette.selected)
            } else {
                (Style::new(), palette.user)
            };
            Line::from(vec![
                Span::styled(label, label_style),
                Span::styled(value, value_style),
            ])
        })
        .collect();

    let mut lines = lines;
    lines.insert(0, settings_tabs(SettingsTab::Appearance, palette));
    lines.insert(1, Line::default());
    let rect = dialog_area(area, lines.len() as u16 + 2);
    let block = Block::bordered()
        .title(" Settings ")
        .padding(Padding::horizontal(1))
        .title_bottom(" ↑↓ select · ←→ change · Tab commands · Esc save and close ");
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The settings dialog's commands tab: every command and key, scrolled by
/// `scroll` lines.
fn draw_commands_tab(frame: &mut Frame, scroll: usize, palette: &Palette, area: Rect) {
    let width = usize::from(area.width.saturating_sub(10).min(94));
    let body: Vec<Line> = describe_help()
        .iter()
        .flat_map(|line| wrap_text(line, width))
        .map(Line::raw)
        .collect();
    let rect = dialog_area(area, area.height);
    let visible = usize::from(rect.height.saturating_sub(4));
    let scroll = scroll.min(body.len().saturating_sub(visible));

    let mut lines = vec![
        settings_tabs(SettingsTab::Commands, palette),
        Line::default(),
    ];
    lines.extend(body.into_iter().skip(scroll));
    let block = Block::bordered()
        .title(" Settings ")
        .padding(Padding::horizontal(1))
        .title_bottom(" ↑↓ scroll · Tab appearance · Esc close ");
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

fn settings_tabs(active: SettingsTab, palette: &Palette) -> Line<'static> {
    let tab = |tab: SettingsTab, label: &'static str| {
        let style = if tab == active {
            palette.selected
        } else {
            palette.muted
        };
        Span::styled(format!(" {label} "), style)
    };
    Line::from(vec![
        tab(SettingsTab::Appearance, "Appearance"),
        Span::raw(" "),
        tab(SettingsTab::Commands, "Commands"),
    ])
}

fn draw_prompt(frame: &mut Frame, dialog: &PromptDialog, palette: &Palette, area: Rect) {
    let rect = dialog_area(area, 3);
    let block = Block::bordered()
        .title(format!(" {} ", dialog.title))
        .padding(Padding::horizontal(1))
        .title_bottom(" Enter save · Esc cancel ")
        .border_style(palette.user);
    let inner = block.inner(rect);
    let width = inner.width.saturating_sub(PROMPT.chars().count() as u16);

    // One line, scrolled so the cursor stays visible.
    let (lines, cursor) = dialog.input.layout(u16::MAX);
    let text = lines.first().cloned().unwrap_or_default();
    let skip = cursor.0.saturating_sub(width.saturating_sub(1));
    let shown: String = text.chars().skip(usize::from(skip)).collect();
    let line = Line::from(vec![Span::styled(PROMPT, palette.user), Span::raw(shown)]);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(line).block(block), rect);

    if inner.height > 0 {
        frame.set_cursor_position(Position {
            x: inner.x + PROMPT.chars().count() as u16 + cursor.0 - skip,
            y: inner.y,
        });
    }
}

fn draw_question(frame: &mut Frame, dialog: &QuestionDialog, palette: &Palette, area: Rect) {
    let questions = dialog.questions();
    let question = &questions[dialog.index];
    let width = usize::from(area.width.saturating_sub(10).min(94));

    let mut lines: Vec<Line> = wrap_text(&question.question, width)
        .into_iter()
        .map(|line| Line::styled(line, Style::new().add_modifier(Modifier::BOLD)))
        .collect();
    lines.push(Line::default());
    for (index, choice) in question.choices.iter().enumerate() {
        let text = truncate_line(&format!("  {}. {choice}", index + 1), width);
        lines.push(if dialog.choice == Some(index) {
            Line::styled(text, palette.selected)
        } else {
            Line::raw(text)
        });
    }
    if !question.choices.is_empty() {
        lines.push(Line::default());
    }
    let (input_lines, cursor) = dialog.input.layout(width.saturating_sub(2) as u16);
    let input_row = lines.len() as u16;
    for (index, line) in input_lines.iter().enumerate() {
        let prompt = if index == 0 { PROMPT } else { "  " };
        let style = if dialog.choice.is_none() {
            Style::new()
        } else {
            palette.muted
        };
        lines.push(Line::from(vec![
            Span::styled(prompt, palette.user),
            Span::styled(line.clone(), style),
        ]));
    }

    let rect = dialog_area(area, lines.len() as u16 + 2);
    let title = format!(" Question {} of {} ", dialog.index + 1, questions.len());
    let block = Block::bordered()
        .title(title)
        .padding(Padding::horizontal(1))
        .title_bottom(" Enter answer · ↑↓ suggestion · Esc later ")
        .border_style(palette.warning);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);

    if dialog.choice.is_none() {
        let y = inner.y + input_row + cursor.1;
        if y < inner.bottom() {
            frame.set_cursor_position(Position {
                x: inner.x + PROMPT.chars().count() as u16 + cursor.0,
                y,
            });
        }
    }
}

#[cfg(test)]
#[path = "../tests/tui/render/tests.rs"]
mod tests;
