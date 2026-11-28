//! The two lines below the input: the working directory with the model,
//! effort and context usage, as the desktop's composer shows them, then the
//! session's agents.
use super::{app::App, text::truncate_line, theme::Palette};
use client_shared::{agent_tree, draft_status_model, format_context_usage, model_display_name};
use psa::api::{Agent, AgentId, AgentState};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

/// Between agents in the bar.
const AGENT_GAP: &str = "  ";
/// Shown at an end of the bar when agents are hidden past it.
const MORE_BEFORE: &str = "‹ ";
const MORE_AFTER: &str = " ›";

pub fn draw_status(frame: &mut Frame, app: &mut App, palette: &Palette, area: Rect) {
    // One column of margin on each side, lining up with the input's border.
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = usize::from(inner.width);
    let location = Rect { height: 1, ..inner };
    frame.render_widget(Paragraph::new(location_line(app, palette, width)), location);
    if inner.height > 1 {
        let agents = Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        };
        let line = agents_line(app, palette, width);
        frame.render_widget(Paragraph::new(line), agents);
    }
}

/// The working directory on the left; the model, colored effort, and context
/// usage on the right.
fn location_line(app: &App, palette: &Palette, width: usize) -> Line<'static> {
    let (path, model, effort, tokens, window) = match (&app.view, &app.draft) {
        (Some(view), _) => {
            let status = app.status();
            let config = status.model_config.as_ref();
            (
                view.session()
                    .map(|session| session.working_directory.display().to_string()),
                config.map(|config| config.model.clone()),
                config.map(|config| config.effort),
                status.context_tokens,
                status.context_window,
            )
        }
        (None, Some(draft)) => {
            let (model, effort) =
                draft_status_model(draft.root.model.as_ref(), draft.model_config.as_ref());
            let window = model
                .as_ref()
                .and_then(|model| app.models.iter().find(|info| info.model == *model))
                .and_then(|info| info.context_window);
            let path = Some(draft.directory.display().to_string());
            (path, model, effort, None, window)
        }
        (None, None) => {
            return Line::styled(truncate_line("No session", width), palette.muted);
        }
    };

    let model = model
        .map(|model| model_display_name(&app.models, &model))
        .unwrap_or_else(|| "Unknown model".into());
    let mut right = vec![Span::raw(model)];
    if let Some(effort) = effort {
        right.push(Span::styled(" · ", palette.muted));
        right.push(Span::styled(effort.as_str(), palette.effort(effort)));
    }
    right.push(Span::styled(
        format!(" {}", format_context_usage(tokens, window)),
        palette.muted,
    ));
    // The details keep up to about two thirds of the line, as on the desktop.
    let right = fit_spans(right, (width * 2 / 3).max(1));

    let path = path.unwrap_or_else(|| "Unknown working directory".into());
    let room = width.saturating_sub(spans_width(&right) + 1);
    let path = truncate_line(&path, room);
    let gap = width.saturating_sub(path.width() + spans_width(&right));
    let mut spans = vec![
        Span::styled(path, palette.muted),
        Span::raw(" ".repeat(gap)),
    ];
    spans.extend(right);
    Line::from(spans)
}

/// The session's name, then its agents with their state, scrolled to keep
/// the open agent visible.
fn agents_line(app: &mut App, palette: &Palette, width: usize) -> Line<'static> {
    let mut extras = Vec::new();
    if let Some(view) = app.view.as_ref().filter(|view| view.connection != "live") {
        extras.push(view.connection.clone());
    }
    if app.details {
        extras.push("details".into());
    }
    let extras = match extras.is_empty() {
        true => String::new(),
        false => format!("  {}", extras.join(" · ")),
    };

    let (session, agents, current) = match (&app.view, &app.draft) {
        (Some(view), _) => {
            let name = view.session().map(|s| s.name.clone()).unwrap_or("…".into());
            let show_hidden = app.settings.preferences.show_hidden_agents;
            let mut agents: Vec<(AgentId, String, AgentState)> = bar_agents(view, show_hidden)
                .into_iter()
                .map(|agent| (agent.id, agent.name, agent.state))
                .collect();
            // The open agent's own stream is the freshest source of its state.
            if let Some(agent) = view.agent() {
                for entry in agents.iter_mut().filter(|entry| entry.0 == agent.id) {
                    entry.2 = agent.state;
                }
            }
            let current = agents.iter().position(|entry| entry.0 == view.agent_id);
            (name, agents, current)
        }
        (None, Some(draft)) => {
            let name = draft.name.clone().unwrap_or_else(|| "New session".into());
            let root = draft.root.selected.as_ref().map(|id| id.name.clone());
            let agents = root
                .map(|name| vec![(AgentId(0), name, AgentState::Idle)])
                .unwrap_or_default();
            let current = (!agents.is_empty()).then_some(0);
            (name, agents, current)
        }
        (None, None) => return Line::default(),
    };

    let session = truncate_line(&session, (width / 3).max(1));
    let prefix = format!("{session} › ");
    let room = width.saturating_sub(prefix.width() + extras.width());
    let mut spans = vec![Span::styled(prefix, palette.muted)];
    if agents.is_empty() {
        spans.push(Span::styled("no agents yet", palette.muted));
    } else {
        let labels: Vec<String> = agents.iter().map(|entry| entry.1.clone()).collect();
        let selected = current.unwrap_or(0);
        let (start, end) = fit_agent_window(&labels, selected, app.agent_bar_offset, room);
        app.agent_bar_offset = start;
        if start > 0 {
            spans.push(Span::styled(MORE_BEFORE, palette.muted));
        }
        for (index, (_, name, state)) in agents.iter().enumerate().take(end).skip(start) {
            if index > start {
                spans.push(Span::raw(AGENT_GAP));
            }
            let dot = match state {
                AgentState::Running => palette.success,
                AgentState::Interrupted => palette.warning,
                AgentState::Idle => palette.muted,
            };
            spans.push(Span::styled("● ", dot));
            let style = match Some(index) == current {
                true => palette.selected,
                false => Style::new(),
            };
            spans.push(Span::styled(name.clone(), style));
        }
        if end < agents.len() {
            spans.push(Span::styled(MORE_AFTER, palette.muted));
        }
    }
    let used = spans_width(&spans);
    if !extras.is_empty() {
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(used + extras.width())),
        ));
        spans.push(Span::styled(extras, palette.muted));
    }
    Line::from(fit_spans(spans, width))
}

/// The agents the bar lists and Alt+[ / Alt+] cycle through, in tree order;
/// the open agent is listed even when hidden.
pub fn bar_agents(view: &super::app::AgentView, show_hidden: bool) -> Vec<Agent> {
    let mut agents: Vec<Agent> = agent_tree(&view.agents, show_hidden)
        .into_iter()
        .map(|row| row.agent)
        .collect();
    if !agents.iter().any(|agent| agent.id == view.agent_id)
        && let Some(open) = view.agents.iter().find(|agent| agent.id == view.agent_id)
    {
        agents.push(open.clone());
    }
    agents
}

/// The agents `[start, end)` shown in `room` columns: starting from the last
/// draw's `offset`, moved just enough to keep `selected` visible, with room
/// for the arrows that mark hidden agents.
pub fn fit_agent_window(
    labels: &[String],
    selected: usize,
    offset: usize,
    room: usize,
) -> (usize, usize) {
    let count = labels.len();
    let selected = selected.min(count.saturating_sub(1));
    let mut start = offset.min(selected);
    loop {
        let end = fill_agent_window(labels, start, room);
        if selected < end || start >= selected {
            return (start, end.max(start + 1).min(count));
        }
        start += 1;
    }
}

/// How far agents from `start` reach in `room` columns; always at least one.
fn fill_agent_window(labels: &[String], start: usize, room: usize) -> usize {
    let count = labels.len();
    let mut used = if start > 0 { MORE_BEFORE.width() } else { 0 };
    let mut end = start;
    while end < count {
        let gap = if end > start { AGENT_GAP.width() } else { 0 };
        // "● " and the name.
        let item = gap + 2 + labels[end].width();
        let arrow = if end + 1 < count {
            MORE_AFTER.width()
        } else {
            0
        };
        if end > start && used + item + arrow > room {
            break;
        }
        used += item;
        end += 1;
    }
    end
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// Cut spans to `width` columns, ending in an ellipsis when cut.
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= width {
        return spans;
    }
    let mut out = Vec::new();
    let mut left = width;
    for span in spans {
        let span_width = span.content.width();
        if span_width < left {
            left -= span_width;
            out.push(span);
            continue;
        }
        let cut = truncate_line(&span.content, left);
        out.push(Span::styled(cut, span.style));
        break;
    }
    out
}

#[cfg(test)]
#[path = "../tests/tui/status_bar/tests.rs"]
mod tests;
