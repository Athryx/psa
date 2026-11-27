//! Plain-text layout helpers measured in terminal columns.
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Word-wrap `text` to `width` columns, breaking words longer than a line.
/// Each source line produces at least one output line.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let source = source.trim_end_matches('\r');
        if source.width() <= width {
            lines.push(source.to_owned());
            continue;
        }
        append_wrapped_line(source, width, &mut lines);
    }
    lines
}

fn append_wrapped_line(source: &str, width: usize, lines: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_width = 0;
    for word in source.split_inclusive(' ') {
        let word_width = word.width();
        if line_width + word_width.saturating_sub(trailing_spaces(word)) <= width {
            line.push_str(word);
            line_width += word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(line.trim_end().to_owned());
            line.clear();
            line_width = 0;
        }
        // A word wider than the line is split at character boundaries.
        for character in word.chars() {
            let character_width = character.width().unwrap_or(0);
            if line_width + character_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
    }
    lines.push(line.trim_end().to_owned());
}

fn trailing_spaces(word: &str) -> usize {
    word.len() - word.trim_end_matches(' ').len()
}

/// Shorten `text` to one line of at most `width` columns, ending in `…` when cut.
pub fn truncate_line(text: &str, width: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    let cut = line.len() < text.trim_end().len();
    if line.width() <= width && !cut {
        return line.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for character in line.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[path = "../tests/tui/text/tests.rs"]
mod tests;
