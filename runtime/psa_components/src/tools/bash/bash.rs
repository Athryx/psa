use super::{
    super::{CodeModeResultType, Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute},
    output::TruncationKind,
};
use crate::{
    bundles::bash::{BashError, BashHandle, CollectedOutput},
    prompts,
};
use llm_api::api::ToolDefinition;
use psa::api::probes::Command;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The bash session's state when this call returned; a later call returns a new
/// result. str() gives the same text as the bash tools.
#[derive(Serialize, JsonSchema)]
pub(super) struct BashResult {
    /// Pass as session_name to interact_bash and close_bash.
    session_name: String,
    /// Output since the previous call; head and tail only when long.
    output: String,
    /// None while running or when the command was killed.
    exit_code: Option<i32>,
    running: bool,
    /// Spawn or signal error.
    error: Option<String>,
    /// Seconds since the command started.
    elapsed: f64,
    /// Seconds this call waited.
    waited: f64,
    /// File holding the full output; None once deleted after a clean run.
    log_path: Option<String>,
}

pub(super) fn bash_result_type() -> CodeModeResultType {
    CodeModeResultType::for_type::<BashResult>()
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    /// Shell command to execute
    command: String,
    /// Wait up to this many milliseconds (default 60000, max 300000); return a session if still running
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(with = "u64")]
    yield_time_ms: Option<f64>,
    /// Optional unique name for a background bash session
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(with = "String")]
    session_name: Option<String>,
    #[serde(default, deserialize_with = "optional_value")]
    #[schemars(skip)]
    yield_timeout_ms: Option<f64>,
}

/// Unlike Option's default deserializer, omitted values are allowed but null is not.
pub(super) fn optional_value<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(super) fn yield_duration(value: Option<f64>, fallback: u64) -> Result<Duration, ToolOutput> {
    let ms = value.unwrap_or(fallback as f64);
    if !ms.is_finite() || ms.fract() != 0.0 || !(0.0..=300_000.0).contains(&ms) {
        return Err(ToolOutput::error(prompts::text(
            "yield_time_ms must be an integer between 0 and 300000",
        )));
    }
    Ok(Duration::from_millis(ms as u64))
}

pub struct Bash {
    handle: BashHandle,
}

impl Bash {
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<BashArgs>(
            "bash",
            "Execute a bash command in the current working directory. Returns stdout and stderr, truncated to the last 2000 lines or 50KB. Waits up to yield_time_ms (default 60000, max 300000); if still running, returns a named background session. Background output is continuously saved to a file.",
            Some(false),
        )
    }

    pub(crate) fn with_handle(handle: BashHandle) -> Self {
        Self { handle }
    }

    async fn execute_typed(
        &self,
        arguments: BashArgs,
        context: &mut AgentContext<'_>,
        cancellation: Cancellation,
    ) -> ToolOutput {
        let wait = match yield_duration(
            arguments.yield_time_ms.or(arguments.yield_timeout_ms),
            60_000,
        ) {
            Ok(wait) => wait,
            Err(error) => return error,
        };
        let handle = &self.handle;
        let probe = match context.probe() {
            Ok(probe) => probe.clone(),
            Err(error) => return ToolOutput::error(prompts::text(error.to_string())),
        };
        // Inherit the remote probe environment, never the server's process environment.
        let command = Command::new("bash", context.session().working_directory.clone())
            .args(["-c".to_owned(), arguments.command]);
        match handle
            .execute(probe, command, arguments.session_name, wait, cancellation)
            .await
        {
            Ok(result) => format_result(result, ResultMode::Initial),
            Err(error) => backend_error(error),
        }
    }
}

impl Tool for Bash {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::Bash
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Execute bash commands; long-running commands yield a session for interact_bash/close_bash".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn codemode_result_type(&self) -> Option<CodeModeResultType> {
        Some(bash_result_type())
    }

    typed_tool_execute!(BashArgs, execute_typed);
}

pub(super) fn backend_error(error: BashError) -> ToolOutput {
    if matches!(error, BashError::Interrupted) {
        return ToolOutput::error(prompts::tool_interrupted());
    }
    ToolOutput::error(prompts::text(
        prompts::truncate_middle(&error.to_string(), 51_200).into_owned(),
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResultMode {
    Initial,
    Interact,
    Close,
}

// JavaScript toFixed rounds exact halfway cases upward, unlike Rust's ties-to-even.
fn one_decimal(value: f64) -> String {
    if matches!(value.fract(), 0.25 | 0.75) {
        let tenths = (value * 10.0).round() as u64;
        format!("{}.{}", tenths / 10, tenths % 10)
    } else {
        format!("{value:.1}")
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", one_decimal(bytes as f64 / 1024.0))
    } else {
        format!("{}MB", one_decimal(bytes as f64 / (1024.0 * 1024.0)))
    }
}

pub(super) fn format_result(result: CollectedOutput, mode: ResultMode) -> ToolOutput {
    let fields = BashResult {
        session_name: result.name.clone(),
        output: result.output.content.clone(),
        exit_code: result.exit_code,
        running: result.running,
        error: result.error.clone(),
        elapsed: result.elapsed.as_secs_f64(),
        waited: result.waited.as_secs_f64(),
        log_path: result
            .log_path
            .as_ref()
            .map(|path| path.display().to_string()),
    };
    format_text(result, mode).with_codemode_fields(&fields)
}

fn format_text(result: CollectedOutput, mode: ResultMode) -> ToolOutput {
    let t = &result.output.truncation;
    let failed = result.error.is_some() || result.exit_code.is_some_and(|code| code != 0);
    let foreground_success =
        mode == ResultMode::Initial && !result.running && !failed && result.exit_code == Some(0);
    let empty = if foreground_success && !t.truncated {
        "(no output)"
    } else {
        "(no new output)"
    };
    let mut text = if result.output.content.is_empty() {
        empty.into()
    } else {
        result.output.content
    };
    let elapsed = format!("{}s", one_decimal(result.elapsed.as_secs_f64()));
    let waited = if mode == ResultMode::Interact {
        format!(
            "; waited: {}s this call",
            one_decimal(result.waited.as_secs_f64())
        )
    } else {
        String::new()
    };
    let truncation = if !t.truncated {
        String::new()
    } else if t.truncated_by == Some(TruncationKind::Lines) {
        format!(
            "Showing lines 1-{} and {}-{} of {}. ",
            t.head_output_lines,
            t.total_lines - t.tail_output_lines + 1,
            t.total_lines,
            t.total_lines
        )
    } else {
        let partial = if t.last_line_partial {
            format!(" (last line is {})", format_size(t.last_line_bytes))
        } else {
            String::new()
        };
        format!(
            "Showing first {} and last {} of {}{partial}. ",
            format_size(t.head_output_bytes),
            format_size(t.tail_output_bytes),
            format_size(t.total_bytes)
        )
    };
    let full_output = match &result.log_path {
        Some(path) => format!("{truncation}Full output: {}", path.display()),
        None => truncation.trim_end().to_owned(),
    };
    // The full output is mentioned when the preview is partial, or when this
    // call returned nothing new because the command had already finished.
    let mention_log = (t.truncated || result.already_finished) && !full_output.is_empty();
    let finished_note = if result.already_finished {
        format!("Bash session {} had already finished. ", result.name)
    } else {
        String::new()
    };

    if result.running {
        let instructions = if mode == ResultMode::Initial {
            " Use interact_bash to send stdin or read new output; close_bash to stop it."
        } else {
            ""
        };
        text.push_str(&format!("\n\n[Running bash session: {}. Command elapsed: {elapsed}{waited}. {full_output}.{instructions}]", result.name));
    } else if result.stopped {
        text.push_str(&format!(
            "\n\n[Closed bash session {}. Command ran for {elapsed}. {full_output}]",
            result.name
        ));
    } else if failed {
        let suffix = if mention_log {
            format!(". {full_output}")
        } else {
            String::new()
        };
        text.push_str(&format!(
            "\n\n[{finished_note}Command ran for {elapsed}{suffix}]\n"
        ));
        if let Some(error) = &result.error {
            text.push_str(&prompts::truncate_middle(error, 51_200));
        } else if let Some(code) = result.exit_code {
            text.push_str(&format!("Command exited with code {code}"));
        }
    } else {
        let suffix = if mention_log {
            format!(" {full_output}")
        } else {
            String::new()
        };
        text.push_str(&format!(
            "\n\n[{finished_note}Command ran for {elapsed}{waited}.{suffix}]"
        ));
    }
    if failed {
        ToolOutput::error(prompts::text(text))
    } else {
        ToolOutput::success(prompts::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bash::output::OutputAccumulator;
    use llm_api::api::{ContentBlock, parse_tool_arguments};
    use serde_json::json;

    fn text(output: ToolOutput) -> (String, bool) {
        let ContentBlock::Text(block) = &output.content[0] else {
            panic!("expected text")
        };
        (block.text.clone(), output.error)
    }

    fn collected(content: &str) -> CollectedOutput {
        let mut accumulator = OutputAccumulator::new();
        accumulator.append(content.as_bytes());
        accumulator.finish();
        CollectedOutput {
            name: "bash1".into(),
            log_path: Some("/tmp/pi-bash-abc.log".into()),
            output: accumulator.take_snapshot(),
            elapsed: Duration::from_millis(12400),
            waited: Duration::from_millis(2400),
            running: false,
            exit_code: Some(0),
            error: None,
            stopped: false,
            already_finished: false,
        }
    }

    #[test]
    fn schema_and_arguments_match_reference() {
        let definition = Bash::definition();
        let schema = serde_json::to_value(definition.parameters).unwrap();
        assert_eq!(schema["required"], json!(["command"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["yield_time_ms"]["type"], "integer");
        assert!(schema["properties"].get("yield_timeout_ms").is_none());
        for value in [
            json!({"command":"x"}),
            json!({"command":"x", "yield_time_ms":0}),
            json!({"command":"x", "yield_timeout_ms":300000}),
        ] {
            assert!(parse_tool_arguments::<BashArgs>(value).is_ok());
        }
        for value in [
            json!({}),
            json!({"command":1}),
            json!({"command":"x", "extra":true}),
            json!({"command":"x", "yield_time_ms":null}),
            json!({"command":"x", "session_name":null}),
            json!({"command":"x", "yield_timeout_ms":null}),
            json!({"command":"x", "yield_time_ms":"0"}),
        ] {
            assert!(parse_tool_arguments::<BashArgs>(value).is_err());
        }
        for value in [-1.0, 0.5, 300001.0, f64::INFINITY, f64::NAN] {
            assert!(yield_duration(Some(value), 60000).is_err());
        }
        assert_eq!(
            yield_duration(None, 60000).ok(),
            Some(Duration::from_secs(60))
        );
        assert_eq!(yield_duration(Some(0.0), 60000).ok(), Some(Duration::ZERO));
        assert_eq!(
            yield_duration(Some(300000.0), 60000).ok(),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn decimal_formatting_and_empty_rules() {
        for (value, expected) in [(0.25, "0.3"), (0.75, "0.8"), (1.15, "1.1"), (12.4, "12.4")] {
            assert_eq!(one_decimal(value), expected);
        }
        assert_eq!(format_size(1280), "1.3KB");
        assert!(
            Bash::with_handle(crate::bundles::bash::BashBundle::new().handle())
                .system_prompt()
                .rules
                .is_empty()
        );
        let output = backend_error(BashError::Interrupted);
        assert!(output.error);
        assert_eq!(
            serde_json::to_value(output.content).unwrap(),
            serde_json::to_value(prompts::tool_interrupted()).unwrap()
        );
    }

    #[test]
    fn source_exact_success_and_running_formats() {
        for (mode, running, expected) in [
            (
                ResultMode::Initial,
                false,
                "(no output)\n\n[Command ran for 12.4s.]",
            ),
            (
                ResultMode::Initial,
                true,
                "(no new output)\n\n[Running bash session: bash1. Command elapsed: 12.4s. Full output: /tmp/pi-bash-abc.log. Use interact_bash to send stdin or read new output; close_bash to stop it.]",
            ),
            (
                ResultMode::Interact,
                false,
                "(no new output)\n\n[Command ran for 12.4s; waited: 2.4s this call.]",
            ),
            (
                ResultMode::Interact,
                true,
                "(no new output)\n\n[Running bash session: bash1. Command elapsed: 12.4s; waited: 2.4s this call. Full output: /tmp/pi-bash-abc.log.]",
            ),
            (
                ResultMode::Close,
                false,
                "(no new output)\n\n[Closed bash session bash1. Command ran for 12.4s. Full output: /tmp/pi-bash-abc.log]",
            ),
        ] {
            let mut result = collected("");
            result.running = running;
            result.stopped = mode == ResultMode::Close;
            assert_eq!(text(format_result(result, mode)), (expected.into(), false));
        }
    }

    #[test]
    fn finished_sessions_report_their_final_state_instead_of_closing() {
        let mut result = collected("");
        result.already_finished = true;
        result.exit_code = Some(3);
        assert_eq!(
            text(format_result(result, ResultMode::Close)),
            (
                "(no new output)\n\n[Bash session bash1 had already finished. Command ran for 12.4s. Full output: /tmp/pi-bash-abc.log]\nCommand exited with code 3".into(),
                true
            )
        );

        let mut result = collected("tail\n");
        result.already_finished = true;
        result.log_path = None;
        assert_eq!(
            text(format_result(result, ResultMode::Interact)),
            (
                "tail\n\n\n[Bash session bash1 had already finished. Command ran for 12.4s; waited: 2.4s this call.]".into(),
                false
            )
        );
    }

    #[test]
    fn source_exact_failure_and_signal_formats() {
        for mode in [ResultMode::Initial, ResultMode::Interact] {
            let mut result = collected("oops");
            result.exit_code = Some(7);
            assert_eq!(
                text(format_result(result, mode)),
                (
                    "oops\n\n[Command ran for 12.4s]\nCommand exited with code 7".into(),
                    true
                )
            );
            let mut result = collected("");
            result.exit_code = None;
            let (output, error) = text(format_result(result, mode));
            assert!(!error);
            let expected = if mode == ResultMode::Initial {
                "(no new output)\n\n[Command ran for 12.4s.]"
            } else {
                "(no new output)\n\n[Command ran for 12.4s; waited: 2.4s this call.]"
            };
            assert_eq!(output, expected);
            let mut result = collected("oops");
            result.error = Some("write failed".into());
            assert_eq!(
                text(format_result(result, mode)),
                ("oops\n\n[Command ran for 12.4s]\nwrite failed".into(), true)
            );
        }
    }

    #[test]
    fn source_exact_truncation_notices_for_all_modes() {
        let input = (1..=2500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for (mode, running, status) in [
            (
                ResultMode::Initial,
                false,
                "[Command ran for 12.4s. Showing lines 1-999 and 1501-2500 of 2500. Full output: /tmp/pi-bash-abc.log]",
            ),
            (
                ResultMode::Initial,
                true,
                "[Running bash session: bash1. Command elapsed: 12.4s. Showing lines 1-999 and 1501-2500 of 2500. Full output: /tmp/pi-bash-abc.log. Use interact_bash to send stdin or read new output; close_bash to stop it.]",
            ),
            (
                ResultMode::Interact,
                false,
                "[Command ran for 12.4s; waited: 2.4s this call. Showing lines 1-999 and 1501-2500 of 2500. Full output: /tmp/pi-bash-abc.log]",
            ),
            (
                ResultMode::Interact,
                true,
                "[Running bash session: bash1. Command elapsed: 12.4s; waited: 2.4s this call. Showing lines 1-999 and 1501-2500 of 2500. Full output: /tmp/pi-bash-abc.log.]",
            ),
            (
                ResultMode::Close,
                false,
                "[Closed bash session bash1. Command ran for 12.4s. Showing lines 1-999 and 1501-2500 of 2500. Full output: /tmp/pi-bash-abc.log]",
            ),
        ] {
            let mut result = collected(&input);
            let preview = result.output.content.clone();
            result.running = running;
            result.stopped = mode == ResultMode::Close;
            assert_eq!(
                text(format_result(result, mode)),
                (format!("{preview}\n\n{status}"), false)
            );
        }
        let result = collected(&"x".repeat(60000));
        let (output, error) = text(format_result(result, ResultMode::Initial));
        assert!(!error);
        assert!(output.ends_with("[Command ran for 12.4s. Showing first 25.0KB and last 25.0KB of 58.6KB (last line is 58.6KB). Full output: /tmp/pi-bash-abc.log]"));
    }
}
