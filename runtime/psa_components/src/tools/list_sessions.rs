use super::{Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};
use crate::prompts;
use llm_api::api::ToolDefinition;
use psa::api::SessionId;
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;

/// Bounds the listing so a large server does not flood the context.
const MAX_OUTPUT_BYTES: usize = 50 * 1024;

pub struct ListSessions;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListSessionsArgs {
    /// List the forks of this session instead of root sessions.
    #[serde(default)]
    parent: Option<u64>,
}

impl ListSessions {
    pub(super) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<ListSessionsArgs>(
            "list_sessions",
            "List psa sessions on this server with their ids, names, working directories, and creation times. Lists root sessions by default, or the forks of `parent`.",
            Some(false),
        )
    }

    async fn execute_typed(
        &self,
        arguments: ListSessionsArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let current = context.session().id;
        let parent = arguments.parent.map(SessionId);
        let sessions = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ToolOutput::error(prompts::tool_interrupted()),
            result = context.runtime().list_sessions(parent) => match result {
                Ok(sessions) => sessions,
                Err(error) => {
                    return ToolOutput::error(prompts::text(format!("Failed to list sessions: {error}")));
                }
            }
        };

        if sessions.is_empty() {
            return ToolOutput::success(prompts::text("No sessions."));
        }
        let lines: Vec<String> = sessions
            .iter()
            .map(|session| {
                let created = session.created_at.format(&Rfc3339).unwrap_or_default();
                let marker = if session.id == current {
                    " (current)"
                } else {
                    ""
                };
                format!(
                    "{}{marker}: {:?} cwd={} created={created}",
                    session.id.0,
                    session.name,
                    session.working_directory.display()
                )
            })
            .collect();
        let listing = lines.join("\n");
        ToolOutput::success(prompts::text(
            prompts::truncate_middle(&listing, MAX_OUTPUT_BYTES).into_owned(),
        ))
    }
}

impl Tool for ListSessions {
    fn id(&self) -> psa::api::ToolId {
        psa::api::ToolId::ListSessions
    }

    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }

    fn system_prompt(&self) -> ToolSystemPrompt {
        ToolSystemPrompt {
            tools: Some("List psa sessions, such as to choose one to export".into()),
            rules: Vec::new(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_tool_execute!(ListSessionsArgs, execute_typed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;

    #[test]
    fn schema_has_an_optional_parent() {
        let definition = ListSessions::definition();
        let parameters = serde_json::to_value(&definition.parameters).unwrap();
        assert_eq!(definition.name, "list_sessions");
        assert!(parameters.get("required").is_none_or(|r| r == &json!([])));

        let parsed: ListSessionsArgs = parse_tool_arguments(json!({})).unwrap();
        assert_eq!(parsed.parent, None);
        assert!(parse_tool_arguments::<ListSessionsArgs>(json!({"parent": "x"})).is_err());
        assert!(parse_tool_arguments::<ListSessionsArgs>(json!({"other": 1})).is_err());
    }
}
