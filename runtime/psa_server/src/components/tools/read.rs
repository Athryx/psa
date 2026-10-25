//! Pi-compatible model-visible read output. Text is counted and streamed in two
//! passes, retaining only a bounded selected prefix; images use Pi's default
//! limits (there is no model-specific inputLimits capability in AgentContext).
use super::{Tool, ToolOutput, ToolSystemPrompt, file_ops, typed_tool_execute};
use crate::{
    agents::agent_context::AgentContext, components::cancellation::Cancellation,
    probes::ProbeHandle, prompts,
};
use llm_api::api::{ContentBlock, ImageBlock, MediaSource, ToolDefinition};
use psa_api::probes::{Command, Event, Stdio};
use psa_shared::probe_transport::ProbeError;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;

#[path = "read_images.rs"]
mod images;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
const SNIFF_BYTES: usize = 4100;

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// Path to the file to read (relative or absolute)
    path: String,
    /// Line number to start reading from (1-indexed)
    #[serde(default, deserialize_with = "deserialize_number")]
    #[schemars(with = "f64")]
    offset: Option<f64>,
    /// Maximum number of lines to read
    #[serde(default, deserialize_with = "deserialize_number")]
    #[schemars(with = "f64")]
    limit: Option<f64>,
}

fn deserialize_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    f64::deserialize(deserializer).map(Some)
}

pub struct Read;

impl Read {
    async fn execute_typed(
        &self,
        arguments: ReadArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let probe = match context.probe() {
            Ok(probe) => probe,
            Err(error) => return io_error(&error, Path::new(&arguments.path)),
        };

        let operation = async {
            let resolved = file_ops::resolve_path(
                probe,
                &arguments.path,
                &context.session().working_directory,
            )
            .await
            .map_err(|error| (error, PathBuf::from(&arguments.path)))?;
            let path = resolve_read_alias(probe, resolved).await?;

            let scanned = scan_file(probe, &path)
                .await
                .map_err(|error| (error, path.clone()))?;
            Ok::<_, (ProbeError, PathBuf)>((path, scanned))
        };
        let (path, scanned) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = operation => match result {
                Ok(result) => result,
                Err((error, path)) => return io_error(&error, &path),
            },
        };

        if let Some(mime) = scanned.mime {
            let bytes = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
                result = file_ops::read_bytes(probe, &path) => match result {
                    Ok(bytes) => bytes,
                    Err(error) => return io_error(&error, &path),
                },
            };
            return read_image(bytes, mime, &mut cancellation).await;
        }

        let selection = match Selection::new(&arguments, scanned.lines) {
            Ok(selection) => selection,
            Err(message) => return ToolOutput::error(prompts::text(message)),
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = read_selection(probe, &path, &selection) => result,
        };
        match result {
            Ok(preview) => ToolOutput::success(prompts::text(preview.format(
                &arguments,
                &selection,
                scanned.lines,
            ))),
            Err(error) => io_error(&error, &path),
        }
    }
}

impl Tool for Read {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::for_type::<ReadArgs>(
            "read",
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            Some(false),
        )
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Read file contents".into()),
            rules: vec!["Use read to examine files instead of cat or sed.".into()],
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    typed_tool_execute!(ReadArgs, execute_typed);
}

fn io_error(error: &ProbeError, path: &Path) -> ToolOutput {
    if matches!(error, ProbeError::Protocol(message) if message == "read cancelled") {
        return ToolOutput::error(prompts::tool_interrupted());
    }
    let operation = match file_ops::parse_remote_error_code(error) {
        Some("ENOENT" | "EACCES" | "EPERM" | "ENOTDIR") => "access",
        _ => "open",
    };
    ToolOutput::error(prompts::text(
        prompts::truncate_middle(&file_ops::format_io_error(error, path, operation), 4096)
            .into_owned(),
    ))
}

// These are alternatives to the original spelling, not cumulative transforms.
fn build_read_aliases(path: &Path) -> Vec<PathBuf> {
    let original = path.to_string_lossy();
    let mut am_pm = String::with_capacity(original.len());
    for (index, character) in original.char_indices() {
        let tail = &original[index..];
        if character == ' '
            && tail
                .get(1..4)
                .is_some_and(|s| s.eq_ignore_ascii_case("am.") || s.eq_ignore_ascii_case("pm."))
        {
            am_pm.push('\u{202f}');
        } else {
            am_pm.push(character);
        }
    }

    let nfd: String = original.nfd().collect();
    [
        am_pm,
        nfd.clone(),
        original.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ]
    .into_iter()
    .filter(|variant| variant != original.as_ref())
    .map(PathBuf::from)
    .collect()
}

async fn resolve_read_alias(
    probe: &ProbeHandle,
    path: PathBuf,
) -> Result<PathBuf, (ProbeError, PathBuf)> {
    for candidate in std::iter::once(path.clone()).chain(build_read_aliases(&path)) {
        let exists = test_path_exists(probe, &candidate)
            .await
            .map_err(|error| (error, path.clone()))?;
        if exists {
            return Ok(candidate);
        }
    }
    Ok(path)
}

async fn test_path_exists(probe: &ProbeHandle, path: &Path) -> Result<bool, ProbeError> {
    // Use the remote shell's test builtin, with paths as positional arguments.
    // Nothing is interpolated into shell source, and output is discarded.
    let command = Command::new("sh", "/")
        .args(["-c", "test -e \"$1\"", "read", path.to_str().unwrap_or("")])
        .stdin(Stdio::Null)
        .stdout(Stdio::Null)
        .stderr(Stdio::Null);
    let mut call = probe.spawn(command).await?;
    loop {
        match call.next_event().await? {
            Event::Complete { status } => {
                return Ok(status.is_some_and(|status| status.code == Some(0)));
            }
            Event::Cancelled => return Err(ProbeError::Protocol("read cancelled".into())),
            _ => {}
        }
    }
}

struct Scanned {
    lines: usize,
    mime: Option<&'static str>,
}

async fn scan_file(probe: &ProbeHandle, path: &Path) -> Result<Scanned, ProbeError> {
    let mut call = probe.read_file(path).await?;
    let mut lines = 1usize;
    let mut sniff = Vec::with_capacity(SNIFF_BYTES);
    let mut sniffed = false;

    loop {
        match call.next_event().await? {
            Event::Data { stream: 1, bytes } => {
                lines = lines.saturating_add(bytes.iter().filter(|&&byte| byte == b'\n').count());
                if sniffed {
                    continue;
                }

                sniff.extend_from_slice(&bytes[..bytes.len().min(SNIFF_BYTES - sniff.len())]);
                if sniff.len() != SNIFF_BYTES {
                    continue;
                }
                if let Some(mime) = images::detect_mime(&sniff) {
                    return Ok(Scanned {
                        lines,
                        mime: Some(mime),
                    });
                }
                sniffed = true;
            }
            Event::Complete { .. } => {
                return Ok(Scanned {
                    lines,
                    mime: images::detect_mime(&sniff),
                });
            }
            Event::Cancelled => return Err(ProbeError::Protocol("read cancelled".into())),
            _ => {}
        }
    }
}

struct Selection {
    // Keep the JS number for displays/notices; Array.slice converts separately.
    start_number: f64,
    start: usize,
    end: usize,
    user_end: Option<f64>,
}

impl Selection {
    fn new(args: &ReadArgs, total: usize) -> Result<Self, String> {
        let start_number = args
            .offset
            .filter(|value| *value != 0.0)
            .map_or(0.0, |value| (value - 1.0).max(0.0));
        if start_number >= total as f64 {
            return Err(format!(
                "Offset {} is beyond end of file ({total} lines total)",
                args.offset.unwrap_or(0.0)
            ));
        }
        let user_end = args
            .limit
            .map(|limit| (start_number + limit).min(total as f64));
        let start = slice_index(start_number, total);
        let end = user_end
            .map_or(total, |end| slice_index(end, total))
            .max(start);
        Ok(Self {
            start_number,
            start,
            end,
            user_end,
        })
    }
}

fn slice_index(value: f64, length: usize) -> usize {
    let integer = value.trunc();
    if integer < 0.0 {
        (length as f64 + integer).max(0.0) as usize
    } else {
        integer.min(length as f64) as usize
    }
}

/// Incremental Buffer.toString("utf-8") equivalent. At most three incomplete
/// bytes survive a chunk boundary; malformed sequences use U+FFFD just as Node.
#[derive(Default)]
struct LossyUtf8 {
    pending: Vec<u8>,
}

impl LossyUtf8 {
    fn push(&mut self, bytes: &[u8], finish: bool, mut output: impl FnMut(&str)) {
        let mut joined = std::mem::take(&mut self.pending);
        let input = if joined.is_empty() {
            bytes
        } else {
            joined.extend_from_slice(bytes);
            &joined
        };

        let mut rest = input;
        while !rest.is_empty() {
            let error = match std::str::from_utf8(rest) {
                Ok(text) => {
                    output(text);
                    break;
                }
                Err(error) => error,
            };
            let valid = error.valid_up_to();
            if valid > 0 {
                output(std::str::from_utf8(&rest[..valid]).unwrap());
            }
            rest = &rest[valid..];

            if let Some(length) = error.error_len() {
                output("\u{fffd}");
                rest = &rest[length..];
                continue;
            }

            if finish {
                output("\u{fffd}");
            } else {
                self.pending.extend_from_slice(rest);
            }
            break;
        }
    }
}

#[derive(Default)]
struct Preview {
    prefix: String,
    bytes: usize,
    first_line_bytes: usize,
    saw_newline: bool,
    ends_newline: bool,
}

impl Preview {
    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        self.bytes = self.bytes.saturating_add(text.len());
        if !self.saw_newline {
            self.first_line_bytes = self
                .first_line_bytes
                .saturating_add(text.find('\n').unwrap_or(text.len()));
            self.saw_newline = text.contains('\n');
        }
        self.ends_newline = text.ends_with('\n');

        let mut keep = text
            .len()
            .min((MAX_BYTES + 4).saturating_sub(self.prefix.len()));
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
        self.prefix.push_str(&text[..keep]);
    }

    fn format(&self, args: &ReadArgs, selection: &Selection, total: usize) -> String {
        let selected_lines = if self.bytes == 0 {
            0
        } else {
            (selection.end - selection.start).saturating_sub(usize::from(self.ends_newline))
        };
        let display = selection.start_number + 1.0;
        if selected_lines <= MAX_LINES && self.bytes <= MAX_BYTES {
            if let Some(end) = selection.user_end.filter(|end| *end < total as f64) {
                return format!(
                    "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                    self.prefix,
                    total as f64 - end,
                    end + 1.0
                );
            }
            return self.prefix.clone();
        }
        if self.first_line_bytes > MAX_BYTES {
            return format!(
                "[Line {display} is {}, exceeds 50.0KB limit. Use bash: sed -n '{display}p' {} | head -c {MAX_BYTES}]",
                format_size(self.first_line_bytes),
                args.path
            );
        }

        let mut kept = Vec::new();
        let mut bytes = 0usize;
        let mut byte_limited = false;
        for line in self.prefix.split('\n').take(selected_lines).take(MAX_LINES) {
            let next = bytes + line.len() + usize::from(!kept.is_empty());
            if next > MAX_BYTES {
                byte_limited = true;
                break;
            }
            bytes = next;
            kept.push(line);
        }

        let end = display + kept.len() as f64 - 1.0;
        let qualifier = if byte_limited { " (50.0KB limit)" } else { "" };
        format!(
            "{}\n\n[Showing lines {display}-{end} of {total}{qualifier}. Use offset={} to continue.]",
            kept.join("\n"),
            end + 1.0
        )
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

async fn read_selection(
    probe: &ProbeHandle,
    path: &Path,
    selection: &Selection,
) -> Result<Preview, ProbeError> {
    let mut preview = Preview::default();
    if selection.end == selection.start {
        return Ok(preview);
    }

    let mut call = probe.read_file(path).await?;
    let mut decoder = LossyUtf8::default();
    let mut line = 0usize;

    loop {
        let event = call.next_event().await?;
        let finished = matches!(event, Event::Complete { .. });
        let bytes = match &event {
            Event::Data { stream: 1, bytes } => bytes.as_slice(),
            Event::Complete { .. } => &[],
            Event::Cancelled => return Err(ProbeError::Protocol("read cancelled".into())),
            _ => continue,
        };

        decoder.push(bytes, finished, |text| {
            for part in text.split_inclusive('\n') {
                let newline = part.ends_with('\n');
                if line >= selection.start && line < selection.end {
                    preview.push(if newline && line + 1 == selection.end {
                        &part[..part.len() - 1]
                    } else {
                        part
                    });
                }
                if newline {
                    line += 1;
                }
            }
        });
        if finished || line >= selection.end {
            return Ok(preview);
        }
    }
}

struct CancelImageOnDrop(Arc<AtomicBool>);

impl Drop for CancelImageOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn read_image(
    bytes: Vec<u8>,
    mime: &'static str,
    cancellation: &mut Cancellation,
) -> ToolOutput {
    let flag = Arc::new(AtomicBool::new(false));
    let cancel_on_drop = CancelImageOnDrop(flag.clone());
    let mut worker = tokio::task::spawn_blocking(move || images::prepare_image(bytes, mime, &flag));
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            cancel_on_drop.0.store(true, Ordering::Relaxed);
            let _ = worker.await;
            return ToolOutput::error(prompts::tool_interrupted());
        },
        result = &mut worker => result.unwrap_or(Err(images::Failure::Resize)),
    };
    let image = match result {
        Ok(image) => image,
        Err(failure) => {
            let message = match failure {
                images::Failure::Conversion => {
                    "[Image omitted: could not be converted to a supported inline image format.]"
                }
                images::Failure::Resize => {
                    "[Image omitted: could not be resized below the inline image size limit.]"
                }
            };
            return ToolOutput::success(prompts::text(format!(
                "Read image file [{mime}]\n{message}"
            )));
        }
    };

    let mut note = format!("Read image file [{}]", image.mime);
    if mime == "image/bmp" {
        note.push_str(&format!(
            "\n[Image converted from {mime} to {}.]",
            image.mime
        ));
    }
    if image.resized {
        note.push_str(&format!("\n[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {:.2} to map to original image.]", image.original_width, image.original_height, image.width, image.height, image.original_width as f64 / image.width as f64));
    }

    let mut content = prompts::text(note);
    content.push(ContentBlock::Image(ImageBlock {
        source: MediaSource::Inline {
            mime_type: image.mime.into(),
            data: image.bytes,
        },
        detail: None,
    }));
    ToolOutput::success(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    fn render(
        bytes: &[u8],
        offset: Option<f64>,
        limit: Option<f64>,
        chunk_size: usize,
    ) -> Result<String, String> {
        let args = ReadArgs {
            path: "file.txt".into(),
            offset,
            limit,
        };
        let total = bytes.iter().filter(|&&byte| byte == b'\n').count() + 1;
        let selection = Selection::new(&args, total)?;
        let mut preview = Preview::default();
        let mut decoder = LossyUtf8::default();
        let mut line = 0usize;
        let mut consume = |text: &str| {
            for part in text.split_inclusive('\n') {
                let newline = part.ends_with('\n');
                if line >= selection.start && line < selection.end {
                    preview.push(if newline && line + 1 == selection.end {
                        &part[..part.len() - 1]
                    } else {
                        part
                    });
                }
                if newline {
                    line += 1;
                }
            }
        };
        for chunk in bytes.chunks(chunk_size) {
            decoder.push(chunk, false, &mut consume);
        }
        decoder.push(&[], true, &mut consume);
        Ok(preview.format(&args, &selection, total))
    }

    #[test]
    fn text_results_match_installed_pi_reference_cases() {
        #[derive(Deserialize)]
        struct Case {
            name: String,
            unit: String,
            repeat: usize,
            tail: String,
            offset: Option<f64>,
            limit: Option<f64>,
            result: Option<String>,
            error: Option<String>,
        }
        let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/pi_read.json")).unwrap();
        for case in cases {
            let input = format!("{}{}", case.unit.repeat(case.repeat), case.tail);
            let expected = match (case.result, case.error) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(error),
                _ => panic!("invalid reference case: {}", case.name),
            };
            for chunk in [7, 4096] {
                let actual = render(input.as_bytes(), case.offset, case.limit, chunk);
                assert_eq!(
                    actual, expected,
                    "Pi read reference: {}, chunk={chunk}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn schema_and_number_arguments_match_pi() {
        let definition = Read.tool_definition();
        let schema = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "read");
        assert_eq!(schema["required"], json!(["path"]));
        assert_eq!(
            schema["properties"]["path"]["description"],
            "Path to the file to read (relative or absolute)"
        );
        assert_eq!(schema["properties"]["offset"]["type"], "number");
        assert_eq!(schema["properties"]["limit"]["type"], "number");
        for value in [
            json!({"path":"x"}),
            json!({"path":"x", "offset":1.5, "limit":-2.25}),
            json!({"path":"x", "extra":true}),
        ] {
            assert!(parse_tool_arguments::<ReadArgs>(value).is_ok());
        }
        for value in [
            json!({}),
            json!({"path":1}),
            json!({"path":"x", "offset":"1"}),
            json!({"path":"x", "offset":null}),
            json!({"path":"x", "limit":null}),
            json!([]),
        ] {
            assert!(parse_tool_arguments::<ReadArgs>(value).is_err());
        }
        assert_eq!(
            Read.system_prompt().tools.as_deref(),
            Some("Read file contents")
        );
        assert_eq!(
            Read.system_prompt().rules,
            ["Use read to examine files instead of cat or sed."]
        );
    }

    #[test]
    fn exact_small_file_offset_limit_and_empty_file_cases() {
        for (source, offset, limit, expected) in [
            ("", None, None, ""),
            ("a\nb\n", None, None, "a\nb\n"),
            ("a\nb\n", Some(2.0), None, "b\n"),
            ("a\nb\n", Some(3.0), None, ""),
            (
                "a\nb\n",
                None,
                Some(1.0),
                "a\n\n[2 more lines in file. Use offset=2 to continue.]",
            ),
            (
                "a\nb\n",
                None,
                Some(0.0),
                "\n\n[3 more lines in file. Use offset=1 to continue.]",
            ),
            (
                "a\nb\n",
                Some(0.0),
                Some(2.0),
                "a\nb\n\n[1 more lines in file. Use offset=3 to continue.]",
            ),
            ("a\nb\nc", Some(-10.0), None, "a\nb\nc"),
            (
                "a\nb\nc",
                None,
                Some(-1.0),
                "a\nb\n\n[4 more lines in file. Use offset=0 to continue.]",
            ),
            (
                "a\nb\nc",
                Some(1.5),
                Some(1.5),
                "a\nb\n\n[1 more lines in file. Use offset=3 to continue.]",
            ),
            (
                "a\nb\nc",
                Some(2.5),
                Some(1.0),
                "b\n\n[0.5 more lines in file. Use offset=3.5 to continue.]",
            ),
        ] {
            assert_eq!(
                render(source.as_bytes(), offset, limit, 1).unwrap(),
                expected,
                "{source:?}, {offset:?}, {limit:?}"
            );
        }
        assert_eq!(
            render(b"a\nb\n", Some(4.0), None, 1).unwrap_err(),
            "Offset 4 is beyond end of file (3 lines total)"
        );
    }

    #[test]
    fn exact_line_and_byte_head_truncation_notices() {
        let text = (0..2001).map(|_| "x").collect::<Vec<_>>().join("\n");
        let expected = format!(
            "{}\n\n[Showing lines 1-2000 of 2001. Use offset=2001 to continue.]",
            vec!["x"; 2000].join("\n")
        );
        assert_eq!(render(text.as_bytes(), None, None, 7).unwrap(), expected);
        let line = "x".repeat(30 * 1024);
        let text = format!("{line}\n{line}\nlast");
        assert_eq!(
            render(text.as_bytes(), None, None, 4096).unwrap(),
            format!("{line}\n\n[Showing lines 1-1 of 3 (50.0KB limit). Use offset=2 to continue.]")
        );
        let text = format!("{}\n", "x".repeat(MAX_BYTES));
        assert_eq!(
            render(text.as_bytes(), None, None, 17).unwrap(),
            format!(
                // Pi removes the trailing empty line before counting, leaving
                // its default truncatedBy="lines" despite the byte overflow.
                "{}\n\n[Showing lines 1-1 of 2. Use offset=2 to continue.]",
                "x".repeat(MAX_BYTES)
            )
        );
        let text = format!("{}\nlast", "x".repeat(MAX_BYTES + 1));
        assert_eq!(
            render(text.as_bytes(), None, None, 4096).unwrap(),
            "[Line 1 is 50.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' file.txt | head -c 51200]"
        );
        assert_eq!(
            render(text.as_bytes(), None, Some(0.0), 4096).unwrap(),
            "\n\n[2 more lines in file. Use offset=1 to continue.]"
        );
    }

    #[test]
    fn trailing_newline_counts_and_limit_precedence_match_pi() {
        let text = "x\n".repeat(2000);
        assert_eq!(render(text.as_bytes(), None, None, 31).unwrap(), text);
        let text = "x\n".repeat(2001);
        assert!(
            render(text.as_bytes(), None, None, 31)
                .unwrap()
                .ends_with("[Showing lines 1-2000 of 2002. Use offset=2001 to continue.]")
        );
        let text = "\n".repeat(2001);
        assert_eq!(
            render(text.as_bytes(), None, None, 17).unwrap(),
            format!(
                "{}\n\n[Showing lines 1-2000 of 2002. Use offset=2001 to continue.]",
                "\n".repeat(1999)
            )
        );
        let text = "x\n".repeat(2100);
        let output = render(text.as_bytes(), Some(2.0), Some(2.0), 29).unwrap();
        assert_eq!(
            output,
            "x\nx\n\n[2098 more lines in file. Use offset=4 to continue.]"
        );
    }

    #[test]
    fn utf8_lossy_is_independent_of_stream_chunk_boundaries() {
        let bytes = b"\xef\xbb\xbfA\xc3\xa9\xf0\x9f\x99\x82\n\xff\xed\xa0\x80\xe2\x82";
        let expected = String::from_utf8_lossy(bytes);
        for size in 1..=bytes.len() {
            assert_eq!(render(bytes, None, None, size).unwrap(), expected);
        }
        let text = format!(
            "{}\n{}",
            "\u{1f642}".repeat(10_000),
            "\u{1f642}".repeat(10_000)
        );
        let output = render(text.as_bytes(), None, None, 3).unwrap();
        assert!(output.starts_with(&"\u{1f642}".repeat(10_000)));
        assert!(
            output.ends_with("[Showing lines 1-1 of 2 (50.0KB limit). Use offset=2 to continue.]")
        );
    }

    #[test]
    fn read_aliases_are_independent_and_preserve_order() {
        let aliases = build_read_aliases(Path::new("/tmp/Caf\u{e9} d'\u{e9}cran 1.00 PM.png"));
        assert_eq!(
            aliases,
            [
                PathBuf::from("/tmp/Caf\u{e9} d'\u{e9}cran 1.00\u{202f}PM.png"),
                PathBuf::from("/tmp/Cafe\u{301} d'e\u{301}cran 1.00 PM.png"),
                PathBuf::from("/tmp/Caf\u{e9} d\u{2019}\u{e9}cran 1.00 PM.png"),
                PathBuf::from("/tmp/Cafe\u{301} d\u{2019}e\u{301}cran 1.00 PM.png"),
            ]
        );
        assert!(build_read_aliases(Path::new("/plain/path")).is_empty());
    }

    #[test]
    fn remote_errors_use_pi_access_and_read_spelling_and_shared_cancellation() {
        for (code, expected) in [
            (
                "ENOENT",
                "ENOENT: no such file or directory, access '/work/file'",
            ),
            ("EACCES", "EACCES: permission denied, access '/work/file'"),
            ("EISDIR", "EISDIR: illegal operation on a directory, read"),
        ] {
            let error = ProbeError::Remote {
                code: code.into(),
                message: String::new(),
            };
            let output = io_error(&error, Path::new("/work/file"));
            assert!(output.error);
            assert!(
                matches!(&output.content[0], ContentBlock::Text(block) if block.text == expected)
            );
        }
        let output = io_error(
            &ProbeError::Protocol("read cancelled".into()),
            Path::new("/work/file"),
        );
        assert!(output.error);
        assert_eq!(
            serde_json::to_value(output.content).unwrap(),
            serde_json::to_value(prompts::tool_interrupted()).unwrap()
        );
    }

    #[tokio::test]
    async fn resized_image_coordinate_note_matches_pi_defaults() {
        use image::{DynamicImage, ImageFormat};
        use std::io::Cursor;
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2001, 2)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let (_sender, mut cancellation) = Cancellation::channel();
        let output = read_image(bytes.into_inner(), "image/png", &mut cancellation).await;
        assert!(!output.error);
        assert!(
            matches!(&output.content[0], ContentBlock::Text(block) if block.text == "Read image file [image/png]\n[Image: original 2001x2, displayed at 2000x2. Multiply coordinates by 1.00 to map to original image.]")
        );
    }

    #[tokio::test]
    async fn image_note_and_inline_bytes_match_pi() {
        use image::{DynamicImage, ImageFormat};
        use std::io::Cursor;
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2, 3)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        let bytes = bytes.into_inner();
        let (_sender, mut cancellation) = Cancellation::channel();
        let output = read_image(bytes.clone(), "image/png", &mut cancellation).await;
        assert!(!output.error);
        assert!(
            matches!(&output.content[0], ContentBlock::Text(block) if block.text == "Read image file [image/png]")
        );
        assert!(
            matches!(&output.content[1], ContentBlock::Image(ImageBlock { source: MediaSource::Inline { data, mime_type }, detail: None }) if data == &bytes && mime_type == "image/png")
        );
        let output = read_image(vec![], "image/png", &mut cancellation).await;
        assert!(
            matches!(&output.content[0], ContentBlock::Text(block) if block.text == "Read image file [image/png]\n[Image omitted: could not be resized below the inline image size limit.]")
        );
        let output = read_image(vec![], "image/bmp", &mut cancellation).await;
        assert!(
            matches!(&output.content[0], ContentBlock::Text(block) if block.text == "Read image file [image/bmp]\n[Image omitted: could not be converted to a supported inline image format.]")
        );
    }
}
