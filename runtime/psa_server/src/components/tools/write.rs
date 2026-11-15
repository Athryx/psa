use super::{
    Tool, ToolOutput, ToolSystemPrompt,
    file_ops::{format_io_error, lock_file, resolve_path, write_bytes},
    typed_tool_execute,
};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation, prompts};
use llm_api::api::ToolDefinition;
use psa_shared::probe_transport::ProbeError;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{path::Path, time::Duration};

pub struct Write;

#[derive(Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct WriteArgs {
    /// Path to the file to write (relative or absolute)
    path: String,
    /// Content to write to the file
    content: String,
}

impl Write {
    async fn execute_typed(
        &self,
        arguments: WriteArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let probe = match context.probe() {
            Ok(probe) => probe,
            Err(error) => return write_error(&error, Path::new(&arguments.path), "open"),
        };

        let path = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return ToolOutput::error(prompts::tool_interrupted());
            }
            result = resolve_path(probe, &arguments.path, &context.session().working_directory) => match result {
                Ok(path) => path,
                Err(error) => return write_error(&error, Path::new(&arguments.path), "open"),
            }
        };

        // Share the mutation queue with edit; cancellation also interrupts lock waits.
        let _guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return ToolOutput::error(prompts::tool_interrupted());
            }
            guard = lock_file(context.session().probe_id, &path) => guard,
        };

        let parent = path.parent().unwrap_or(&path);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return ToolOutput::error(prompts::tool_interrupted());
            }
            result = probe.create_dir(parent) => {
                if let Err(error) = result {
                    return write_error(&error, parent, "mkdir");
                }
            }
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return ToolOutput::error(prompts::tool_interrupted());
            }
            result = write_bytes(probe, &path, arguments.content.as_bytes()) => {
                if let Err(error) = result {
                    return write_error(&error, &path, "open");
                }
            }
        }

        ToolOutput::success(prompts::text(format!(
            "Successfully wrote to {}",
            arguments.path
        )))
    }
}

// The shared formatter bounds errors to 50 KiB on UTF-8 boundaries.
fn write_error(error: &ProbeError, path: &Path, operation: &str) -> ToolOutput {
    ToolOutput::error(prompts::text(format_io_error(error, path, operation)))
}

impl Tool for Write {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::for_type::<WriteArgs>(
            "write",
            "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
            Some(false),
        )
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("Create or overwrite files".into()),
            rules: vec!["Use write only for new files or complete rewrites.".into()],
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    typed_tool_execute!(WriteArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::{ContentBlock, parse_tool_arguments};
    use serde_json::json;

    #[test]
    fn schema_and_prompts_match_pi() {
        let definition = Write.tool_definition();
        let parameters = serde_json::to_value(&definition.parameters).unwrap();

        assert_eq!(definition.name, "write");
        assert_eq!(
            definition.description,
            "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
        );
        assert_eq!(definition.strict, Some(false));
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(parameters["required"], json!(["path", "content"]));
        assert_eq!(parameters["properties"]["path"]["type"], "string");
        assert_eq!(parameters["properties"]["content"]["type"], "string");
        assert_eq!(
            parameters["properties"]["path"]["description"],
            "Path to the file to write (relative or absolute)"
        );
        assert_eq!(
            parameters["properties"]["content"]["description"],
            "Content to write to the file"
        );
        assert_eq!(
            Write.system_prompt(),
            ToolSystemPrompt {
                tools: Some("Create or overwrite files".into()),
                rules: vec!["Use write only for new files or complete rewrites.".into()],
            }
        );
        assert_eq!(Write.cancellation_timeout(), Duration::from_secs(3));
        assert!(
            definition
                .parameters
                .validate_openai_parameters(definition.strict)
                .is_ok()
        );
    }

    #[test]
    fn arguments_require_two_strings_and_allow_extra_fields() {
        let arguments = parse_tool_arguments::<WriteArgs>(json!({
            "path": "@nested/文件.txt",
            "content": "hello\n世界"
        }))
        .unwrap();
        assert_eq!(arguments.path, "@nested/文件.txt");
        assert_eq!(arguments.content, "hello\n世界");
        assert!(parse_tool_arguments::<WriteArgs>(json!({"path": "", "content": ""})).is_ok());
        assert!(
            parse_tool_arguments::<WriteArgs>(
                json!({"path": "file", "content": "text", "extra": true})
            )
            .is_ok()
        );

        for arguments in [
            json!({}),
            json!({"path": "file"}),
            json!({"content": "text"}),
            json!({"path": null, "content": "text"}),
            json!({"path": "file", "content": null}),
            json!({"path": 1, "content": "text"}),
            json!({"path": "file", "content": ["text"]}),
            json!([]),
            json!(null),
        ] {
            assert!(parse_tool_arguments::<WriteArgs>(arguments).is_err());
        }
    }

    #[test]
    fn error_preview_is_byte_bounded_and_retains_unicode_ends() {
        let error = ProbeError::Protocol(format!("start{}end", "界".repeat(30_000)));
        let output = write_error(&error, Path::new("/tmp/文件"), "open");
        assert!(output.error);
        let [ContentBlock::Text(block)] = output.content.as_slice() else {
            panic!("expected one text block");
        };
        let full = format_io_error(&error, Path::new("/tmp/文件"), "open");
        assert!(block.text.len() <= 50 * 1024);
        assert!(block.text.contains("[... middle truncated ...]"));
        assert!(block.text.starts_with(&full[..full.find('界').unwrap()]));
        assert!(
            block
                .text
                .ends_with(&full[full.rfind('界').unwrap() + '界'.len_utf8()..])
        );
    }
}
