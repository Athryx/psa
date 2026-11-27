//! Keyboard and paste handling.
use super::{
    app::{App, Overlay, QuestionDialog, SettingsTab},
    composer::Composer,
    settings::SettingRow,
};
use client_shared::{CandidateKind, ComposerInput, Frontend, parse_composer_input};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

impl App {
    pub fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Paste(text) => match &mut self.overlay {
                Some(Overlay::Question(dialog)) => {
                    dialog.input.insert_str(&text);
                    dialog.choice = None;
                }
                // A name is one line.
                Some(Overlay::Prompt(dialog)) => dialog.input.insert_str(&text.replace('\n', " ")),
                Some(_) => {}
                None => {
                    self.composer.insert_str(&text);
                    self.completion.reset();
                }
            },
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            self.handle_interrupt_key();
            return;
        }
        self.quit_armed = false;

        if self.overlay.is_some() {
            self.handle_overlay_key(key);
            return;
        }
        if self.handle_view_key(key) {
            return;
        }
        if self.handle_completion_key(key) {
            return;
        }
        self.handle_composer_key(key);
    }

    /// Ctrl-C closes a dialog, then clears the input, then quits when pressed twice.
    fn handle_interrupt_key(&mut self) {
        if matches!(self.overlay, Some(Overlay::Settings { .. })) {
            self.close_settings();
            return;
        }
        if self.overlay.take().is_some() {
            return;
        }
        if !self.composer.is_empty() {
            self.clear_composer();
            return;
        }
        if self.quit_armed {
            self.quit = true;
            return;
        }
        self.quit_armed = true;
        self.set_notice("Press Ctrl+C again to quit.");
    }

    /// Keys acting on the transcript or agent rather than the input.
    fn handle_view_key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.page_height.saturating_sub(2).max(1);
        match key.code {
            KeyCode::Char('o') if control => self.details = !self.details,
            KeyCode::Char('q') if control => self.reopen_question(),
            KeyCode::PageUp => self.scroll += page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Up if shift => self.scroll += 1,
            KeyCode::Down if shift => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::End if control => self.scroll = 0,
            KeyCode::Esc if self.command_menu().is_none() => self.handle_escape(),
            _ => return false,
        }
        true
    }

    /// Esc interrupts a running agent, otherwise clears the status message.
    fn handle_escape(&mut self) {
        let Some(view) = self.view.as_ref().filter(|view| view.is_running()) else {
            self.message = None;
            return;
        };
        self.network.interrupt(view.session_id, view.agent_id);
    }

    /// Keys for the command menu; false when no menu is open or the key is
    /// not one it uses.
    fn handle_completion_key(&mut self, key: KeyEvent) -> bool {
        let Some(menu) = self.command_menu() else {
            return false;
        };
        let count = menu.candidates.len();
        let selected = menu.candidates.get(self.completion.index(count)).cloned();
        match key.code {
            KeyCode::Up => self.completion.previous(count),
            KeyCode::Down => self.completion.next(count),
            KeyCode::Esc => self.completion.dismiss(),
            KeyCode::Tab => {
                let Some(candidate) = selected else {
                    return true;
                };
                if candidate.kind == CandidateKind::Toggle {
                    self.apply_menu_action(candidate.action);
                } else if let Some(text) = candidate.completion {
                    self.composer.set_text(text);
                    self.select_candidate(&candidate.label);
                }
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(candidate) = selected {
                    self.apply_menu_action(candidate.action);
                }
            }
            _ => return false,
        }
        true
    }

    /// Highlight the candidate labelled `label` in the rebuilt menu, so a
    /// completed choice stays highlighted.
    fn select_candidate(&mut self, label: &str) {
        self.completion.reset();
        let index = self
            .command_menu()
            .and_then(|menu| menu.candidates.iter().position(|c| c.label == label));
        self.completion.selected = index.unwrap_or(0);
    }

    fn handle_composer_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if newline => self.composer.insert_char('\n'),
            KeyCode::Char('j') if control => self.composer.insert_char('\n'),
            KeyCode::Enter => self.submit(),
            _ if apply_edit_key(&mut self.composer, key) => {}
            _ => return,
        }
        // Editing reopens a dismissed command menu; leaving `/model` collapses
        // its list back to pinned models.
        self.completion.reset();
        if !self.composer.text().starts_with("/model ") {
            self.show_all_models = false;
        }
    }

    fn submit(&mut self) {
        let text = self.composer.text().to_owned();
        if text.trim().is_empty() {
            return;
        }
        match parse_composer_input(&text, Frontend::Tui) {
            ComposerInput::Command { id, arguments } => {
                self.clear_composer();
                self.run_command(id, arguments);
            }
            ComposerInput::Text(content) if self.draft.is_some() => self.submit_draft(content),
            ComposerInput::Text(content) => {
                let Some(view) = &self.view else {
                    self.set_error("Open a session first: /sessions or /new.");
                    return;
                };
                self.network
                    .send_message(view.session_id, view.agent_id, content);
                self.clear_composer();
                self.scroll = 0;
                self.message = None;
            }
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        match overlay {
            Overlay::Picker(picker) => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down | KeyCode::Tab => picker.select_next(),
                KeyCode::Backspace => picker.pop_filter(),
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    picker.push_filter(character)
                }
                KeyCode::Enter => {
                    if let Some(value) = picker.selected_value() {
                        self.choose(value);
                    }
                }
                _ => {}
            },
            Overlay::Panel { scroll, .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll += 1,
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll += 10,
                _ => {}
            },
            Overlay::Prompt(dialog) => match key.code {
                KeyCode::Esc => self.overlay = None,
                KeyCode::Enter => {
                    let value = dialog.input.text().trim().to_owned();
                    if value.is_empty() {
                        return;
                    }
                    let action = dialog.action;
                    self.overlay = None;
                    self.submit_prompt(action, value);
                }
                _ => {
                    apply_edit_key(&mut dialog.input, key);
                }
            },
            Overlay::Settings {
                tab,
                selected,
                changed,
            } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.close_settings();
                    return;
                }
                if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                    *tab = match tab {
                        SettingsTab::Appearance => SettingsTab::Commands,
                        SettingsTab::Commands => SettingsTab::Appearance,
                    };
                    *selected = 0;
                    return;
                }
                if *tab == SettingsTab::Commands {
                    match key.code {
                        KeyCode::Up => *selected = selected.saturating_sub(1),
                        KeyCode::Down => *selected += 1,
                        KeyCode::PageUp => *selected = selected.saturating_sub(10),
                        KeyCode::PageDown => *selected += 10,
                        _ => {}
                    }
                    return;
                }
                let count = SettingRow::ALL.len();
                let forward = match key.code {
                    KeyCode::Up => {
                        *selected = (*selected + count - 1) % count;
                        return;
                    }
                    KeyCode::Down => {
                        *selected = (*selected + 1) % count;
                        return;
                    }
                    KeyCode::Left => false,
                    KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => true,
                    _ => return,
                };
                *changed = true;
                SettingRow::ALL[*selected].cycle(&mut self.settings.preferences, forward);
            }
            Overlay::Question(dialog) => {
                if key.code == KeyCode::Esc {
                    let id = dialog.request.id;
                    self.dismissed_queries.push(id);
                    self.overlay = None;
                    self.set_notice("Question left unanswered; Ctrl+Q reopens it.");
                    return;
                }
                if let Some(answers) = handle_question_key(dialog, key) {
                    let request = &dialog.request;
                    self.network.answer_query(
                        request.session_id,
                        request.agent_id,
                        request.id,
                        answers,
                    );
                    self.overlay = None;
                }
            }
        }
    }
}

/// Edit the current answer; returns every answer once the last one is accepted.
fn handle_question_key(dialog: &mut QuestionDialog, key: KeyEvent) -> Option<Vec<String>> {
    let choices = dialog.questions()[dialog.index].choices.len();
    match key.code {
        KeyCode::Up if choices > 0 => {
            dialog.choice = match dialog.choice {
                None => Some(choices - 1),
                Some(0) => None,
                Some(index) => Some(index - 1),
            };
        }
        KeyCode::Down if choices > 0 => {
            dialog.choice = match dialog.choice {
                None => Some(0),
                Some(index) if index + 1 < choices => Some(index + 1),
                Some(_) => None,
            };
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
            dialog.input.insert_char('\n')
        }
        KeyCode::Enter => return accept_answer(dialog),
        _ => {
            if apply_edit_key(&mut dialog.input, key) {
                dialog.choice = None;
            }
        }
    }
    None
}

/// Line-editing keys shared by every input; false when `key` is not one.
fn apply_edit_key(input: &mut Composer, key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('a') if control => input.move_line_start(),
        KeyCode::Char('e') if control => input.move_line_end(),
        KeyCode::Char('u') if control => input.clear(),
        KeyCode::Char('w') if control => input.delete_word_before(),
        KeyCode::Backspace if alt => input.delete_word_before(),
        KeyCode::Char(character) if !control => input.insert_char(character),
        KeyCode::Backspace => input.delete_before(),
        KeyCode::Delete => input.delete_after(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_line_start(),
        KeyCode::End => input.move_line_end(),
        _ => return false,
    }
    true
}

fn accept_answer(dialog: &mut QuestionDialog) -> Option<Vec<String>> {
    let question = &dialog.questions()[dialog.index];
    let answer = match dialog.choice {
        Some(index) => question.choices[index].clone(),
        None => dialog.input.text().trim().to_owned(),
    };
    if answer.is_empty() {
        return None;
    }

    dialog.answers.push(answer);
    dialog.index += 1;
    dialog.input.clear();
    dialog.choice = None;
    (dialog.index == dialog.questions().len()).then(|| std::mem::take(&mut dialog.answers))
}

#[cfg(test)]
#[path = "../tests/tui/input/tests.rs"]
mod tests;
