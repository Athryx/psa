use super::{
    Tool, ToolOutput, ToolSystemPrompt,
    file_ops::{format_io_error, resolve_path, write_bytes},
    typed_tool_execute,
};
use crate::{prompts, trace_export};
use agent_trace::store::SESSION_FILE;
use llm_api::api::ToolDefinition;
use psa::api::SessionId;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{path::Path, time::Duration};

pub struct ExportTrace;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExportTraceArgs {
    /// Directory to write the trace into (relative or absolute). Created if missing.
    path: String,
    /// Session to export; defaults to the current session. Use list_sessions to find ids.
    #[serde(default)]
    session_id: Option<u64>,
    /// Replace a trace already in the directory. Defaults to false.
    #[serde(default)]
    overwrite: bool,
}

impl ExportTrace {
    pub(super) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<ExportTraceArgs>(
            "export_trace",
            "Export a psa session, including all of its agents, as a trace directory: session.json with metadata and tool definitions, agents/<id>.jsonl with each agent's events, and blobs/ with images. The `trace` CLI can then summarize, render, and search it.",
            Some(false),
        )
    }

    async fn execute_typed(
        &self,
        arguments: ExportTraceArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let probe = match context.probe() {
            Ok(probe) => probe.clone(),
            Err(error) => return probe_error(&error, Path::new(&arguments.path), "open"),
        };
        let session_id = arguments
            .session_id
            .map(SessionId)
            .unwrap_or(context.session().id);

        let dir = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = resolve_path(&probe, &arguments.path, &context.session().working_directory) => match result {
                Ok(dir) => dir,
                Err(error) => return probe_error(&error, Path::new(&arguments.path), "open"),
            }
        };

        // A missing directory is fine; an existing trace needs explicit permission.
        if !arguments.overwrite
            && let Ok(entries) = probe.list_dir(&dir).await
            && entries.iter().any(|entry| entry.name == SESSION_FILE)
        {
            return ToolOutput::error(prompts::text(format!(
                "{} already contains a trace. Choose another path or set overwrite to true.",
                dir.display()
            )));
        }

        let mut trace = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = trace_export::export_session(context.runtime(), session_id) => match result {
                Ok(trace) => trace,
                Err(error) => {
                    return ToolOutput::error(prompts::text(format!(
                        "Failed to export session {}: {error}",
                        session_id.0
                    )));
                }
            }
        };

        for file in trace.to_files() {
            if cancellation.is_cancelled() {
                return ToolOutput::error(prompts::tool_interrupted());
            }
            let path = dir.join(&file.path);
            let parent = path.parent().unwrap_or(&dir);
            if let Err(error) = probe.create_dir(parent).await {
                return probe_error(&error, parent, "mkdir");
            }
            if let Err(error) = write_bytes(&probe, &path, &file.contents).await {
                return probe_error(&error, &path, "open");
            }
        }

        let events: usize = trace.agents.iter().map(|agent| agent.events.len()).sum();
        ToolOutput::success(prompts::text(format!(
            "Exported session {} to {}: {} agents, {events} events, {} images.",
            session_id.0,
            dir.display(),
            trace.agents.len(),
            trace.blobs.len()
        )))
    }
}

// The shared formatter bounds errors to 50 KiB on UTF-8 boundaries.
fn probe_error(
    error: &psa_shared::probe_transport::ProbeError,
    path: &Path,
    operation: &str,
) -> ToolOutput {
    ToolOutput::error(prompts::text(format_io_error(error, path, operation)))
}

impl Tool for ExportTrace {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::ExportTrace
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Save a psa session as a trace directory for analysis".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    typed_tool_execute!(ExportTraceArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn schema_requires_only_the_path() {
        let definition = ExportTrace::definition();
        let parameters = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "export_trace");
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(parameters["additionalProperties"], false);

        let parsed: ExportTraceArgs =
            parse_tool_arguments(json!({"path": "out", "session_id": 3})).unwrap();
        assert_eq!(parsed.session_id, Some(3));
        assert!(!parsed.overwrite);
        assert!(
            parse_tool_arguments::<ExportTraceArgs>(json!({"path": "out", "extra": 1})).is_err()
        );
        assert!(parse_tool_arguments::<ExportTraceArgs>(json!({"session_id": 3})).is_err());
    }
}
