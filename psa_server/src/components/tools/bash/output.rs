//! Bounded interval previews. The backend, not this accumulator, owns the full log.
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
const ROLLING_BYTES: usize = MAX_BYTES * 2;
const MIDDLE_MARKER: &str = "\n... [middle output omitted] ...\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TruncationKind {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Truncation {
    pub truncated: bool,
    pub truncated_by: Option<TruncationKind>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub head_output_lines: usize,
    pub tail_output_lines: usize,
    pub head_output_bytes: usize,
    pub tail_output_bytes: usize,
    pub last_line_partial: bool,
    pub last_line_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OutputSnapshot {
    pub content: String,
    pub truncation: Truncation,
}

#[derive(Default)]
pub(crate) struct OutputAccumulator {
    head: String,
    head_complete: bool,
    tail: String,
    pending: Vec<u8>,
    total_bytes: usize,
    completed_lines: usize,
    current_line_bytes: usize,
    last_completed_line_bytes: usize,
    finished: bool,
    decoded_any: bool,
}

impl OutputAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        assert!(
            !self.finished,
            "Cannot append to a finished output accumulator"
        );
        // Never copy an arbitrarily large incoming data event into scratch space.
        for chunk in bytes.chunks(4096) {
            self.pending.extend_from_slice(chunk);
            self.decode(false);
        }
    }

    pub(crate) fn finish(&mut self) {
        if !self.finished {
            self.decode(true);
            self.finished = true;
        }
    }

    fn decode(&mut self, final_chunk: bool) {
        let bytes = std::mem::take(&mut self.pending);
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(text) => {
                    self.append_text(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    self.append_text(std::str::from_utf8(&remaining[..valid]).unwrap());
                    remaining = &remaining[valid..];
                    match error.error_len() {
                        Some(length) => {
                            self.append_text("\u{fffd}");
                            remaining = &remaining[length..];
                        }
                        None if final_chunk => {
                            self.append_text("\u{fffd}");
                            break;
                        }
                        None => {
                            self.pending.extend_from_slice(remaining);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn append_text(&mut self, mut text: &str) {
        if text.is_empty() {
            return;
        }
        // Match TextDecoder's initial BOM handling, without resetting at polls.
        if !self.decoded_any {
            self.decoded_any = true;
            text = text.strip_prefix('\u{feff}').unwrap_or(text);
        }
        self.total_bytes += text.len();
        for part in text.split_inclusive('\n') {
            if let Some(line) = part.strip_suffix('\n') {
                self.last_completed_line_bytes = self.current_line_bytes + line.len();
                self.current_line_bytes = 0;
                self.completed_lines += 1;
            } else {
                self.current_line_bytes += part.len();
            }
        }
        if !self.head_complete {
            let available = ROLLING_BYTES - self.head.len();
            let beginning = prefix(text, available);
            self.head.push_str(beginning);
            self.head_complete = beginning.len() < text.len() || self.head.len() == ROLLING_BYTES;
        }
        self.tail.push_str(text);
        if self.tail.len() > ROLLING_BYTES {
            let mut start = self.tail.len() - ROLLING_BYTES;
            while !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail.drain(..start);
        }
    }

    /// Resets only the displayed interval. Incomplete UTF-8 survives a poll.
    pub(crate) fn take_snapshot(&mut self) -> OutputSnapshot {
        let total_lines = self.completed_lines + usize::from(self.current_line_bytes > 0);
        let mut truncation = Truncation {
            total_lines,
            total_bytes: self.total_bytes,
            last_line_bytes: if self.current_line_bytes > 0 {
                self.current_line_bytes
            } else {
                self.last_completed_line_bytes
            },
            ..Truncation::default()
        };
        let content = if total_lines <= MAX_LINES && self.total_bytes <= MAX_BYTES {
            std::mem::take(&mut self.head)
        } else {
            truncation.truncated = true;
            truncation.truncated_by = Some(if self.total_bytes > MAX_BYTES {
                TruncationKind::Bytes
            } else {
                TruncationKind::Lines
            });
            let content_bytes = MAX_BYTES - MIDDLE_MARKER.len();
            let head_budget = content_bytes / 2;
            let tail_budget = content_bytes - head_budget;
            let (head, head_lines, _) = select_end(&self.head, 999, head_budget, false);
            let (tail, tail_lines, partial) = select_end(&self.tail, 1000, tail_budget, true);
            truncation.head_output_lines = head_lines;
            truncation.tail_output_lines = tail_lines;
            truncation.head_output_bytes = head.len();
            truncation.tail_output_bytes = tail.len();
            truncation.last_line_partial = partial;
            format!("{head}{MIDDLE_MARKER}{tail}")
        };
        self.head.clear();
        self.head_complete = false;
        self.tail.clear();
        self.total_bytes = 0;
        self.completed_lines = 0;
        self.current_line_bytes = 0;
        self.last_completed_line_bytes = 0;
        OutputSnapshot {
            content,
            truncation,
        }
    }
}

fn prefix(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn select_end(text: &str, max_lines: usize, max_bytes: usize, tail: bool) -> (String, usize, bool) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') || text.is_empty() {
        lines.pop();
    }
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return (text.into(), lines.len(), false);
    }
    if tail {
        lines.reverse();
    }
    let mut selected = Vec::new();
    let mut bytes = 0;
    for line in lines.into_iter().take(max_lines) {
        let size = line.len() + usize::from(!selected.is_empty());
        if bytes + size > max_bytes {
            if selected.is_empty() {
                if !tail {
                    return (prefix(text, max_bytes).into(), 1, true);
                }
                let mut start = line.len().saturating_sub(max_bytes);
                while !line.is_char_boundary(start) {
                    start += 1;
                }
                return (line[start..].into(), 1, true);
            }
            break;
        }
        selected.push(line);
        bytes += size;
    }
    if tail {
        selected.reverse();
    }
    (selected.join("\n"), selected.len(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_line_fixture_and_interval_reset() {
        let input = (1..=2500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = OutputAccumulator::new();
        output.append(input.as_bytes());
        let snapshot = output.take_snapshot();
        let expected = format!(
            "{}{}{}",
            (1..=999)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            MIDDLE_MARKER,
            (1501..=2500)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(snapshot.content, expected);
        assert_eq!(snapshot.truncation.total_lines, 2500);
        assert_eq!(snapshot.truncation.head_output_lines, 999);
        assert_eq!(snapshot.truncation.tail_output_lines, 1000);
        assert_eq!(
            snapshot.truncation.truncated_by,
            Some(TruncationKind::Lines)
        );
        assert!(output.take_snapshot().content.is_empty());
    }

    #[test]
    fn streaming_utf8_survives_poll_and_finish() {
        let mut output = OutputAccumulator::new();
        output.append(b"a\xf0\x9f");
        assert_eq!(output.take_snapshot().content, "a");
        output.append(b"\x99\x82\xff\xe2\x82");
        output.finish();
        output.finish();
        assert_eq!(output.take_snapshot().content, "🙂\u{fffd}\u{fffd}");
    }

    #[test]
    fn memory_and_preview_are_bounded_for_unlimited_output() {
        let mut output = OutputAccumulator::new();
        let chunk = "🙂".repeat(50000);
        for _ in 0..100 {
            output.append(chunk.as_bytes());
            assert!(output.head.len() <= ROLLING_BYTES);
            assert!(output.tail.len() <= ROLLING_BYTES);
            assert!(output.pending.len() <= 3);
        }
        let snapshot = output.take_snapshot();
        assert!(snapshot.content.len() <= MAX_BYTES);
        assert!(snapshot.content.contains(MIDDLE_MARKER));
        assert_eq!(snapshot.truncation.total_bytes, chunk.len() * 100);
        assert_eq!(
            snapshot.truncation.truncated_by,
            Some(TruncationKind::Bytes)
        );
        assert!(snapshot.truncation.last_line_partial);
    }

    #[test]
    fn head_never_skips_a_character_when_rolling_budget_splits_utf8() {
        let mut output = OutputAccumulator::new();
        output.append("x".repeat(ROLLING_BYTES - 1).as_bytes());
        output.append("🙂".as_bytes());
        output.append(b"z");
        assert_eq!(output.head, "x".repeat(ROLLING_BYTES - 1));
        assert!(output.tail.ends_with("🙂z"));
    }

    #[test]
    fn small_intervals_preserve_trailing_newlines_and_exact_limits() {
        for input in [
            "",
            "a",
            "a\n",
            "\n\n",
            &"x\n".repeat(MAX_LINES),
            &"x".repeat(MAX_BYTES),
        ] {
            let mut output = OutputAccumulator::new();
            output.append(input.as_bytes());
            output.finish();
            let snapshot = output.take_snapshot();
            assert_eq!(snapshot.content, input);
            assert!(!snapshot.truncation.truncated);
        }
    }

    #[test]
    fn invalid_utf8_and_line_counts_do_not_depend_on_chunks() {
        let input = b"\xef\xbb\xbfhello\xc3\xa9\n\xff\xed\xa0\x80\xe2\x82";
        for size in 1..=input.len() {
            let mut output = OutputAccumulator::new();
            for bytes in input.chunks(size) {
                output.append(bytes);
            }
            output.finish();
            let snapshot = output.take_snapshot();
            assert_eq!(snapshot.content, String::from_utf8_lossy(&input[3..]));
            assert_eq!(snapshot.truncation.total_lines, 2);
        }
    }
}
