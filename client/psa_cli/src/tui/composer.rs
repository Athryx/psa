//! A small multi-line text editor for the message input and dialogs.
use unicode_width::UnicodeWidthChar;

/// Text with a cursor kept on a character boundary.
#[derive(Debug, Default, Clone)]
pub struct Composer {
    text: String,
    cursor: usize,
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.take();
    }

    pub fn insert_char(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    /// Insert pasted text, normalizing line endings.
    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn delete_before(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
    }

    pub fn delete_after(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    /// Delete the word before the cursor and the spaces after it, like Ctrl-W in a shell.
    pub fn delete_word_before(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .rfind(char::is_whitespace)
            .map(|index| index + 1)
            .unwrap_or(0);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    pub fn move_line_start(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_line_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.text.len());
    }

    /// Wrap at `width` columns by character, returning the lines and the
    /// cursor's (column, row).
    pub fn layout(&self, width: u16) -> (Vec<String>, (u16, u16)) {
        let width = usize::from(width.max(1));
        let mut lines = vec![String::new()];
        let mut column = 0;
        let mut cursor = (0, 0);
        for (index, character) in self.text.char_indices() {
            if index == self.cursor {
                cursor = (column, lines.len() - 1);
            }
            if character == '\n' {
                lines.push(String::new());
                column = 0;
                continue;
            }
            let character_width = character.width().unwrap_or(0);
            if column + character_width > width {
                lines.push(String::new());
                column = 0;
                if index == self.cursor {
                    cursor = (0, lines.len() - 1);
                }
            }
            lines.last_mut().unwrap().push(character);
            column += character_width;
        }
        if self.cursor == self.text.len() {
            cursor = if column >= width {
                lines.push(String::new());
                (0, lines.len() - 1)
            } else {
                (column, lines.len() - 1)
            };
        }
        let position = (cursor.0 as u16, cursor.1 as u16);
        (lines, position)
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|character| self.cursor + character.len_utf8())
    }
}

#[cfg(test)]
#[path = "../tests/tui/composer/tests.rs"]
mod tests;
