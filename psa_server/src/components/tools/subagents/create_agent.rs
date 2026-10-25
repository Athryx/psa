use super::{format_tool_output, require_coordinator};
use crate::{
    agents::agent_context::AgentContext,
    components::{
        bundles::subagents::SubagentCoordinator,
        cancellation::Cancellation,
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
    prompts,
};
use llm_api::api::ToolDefinition;
use psa_api::{AgentMessageSource, ModelSelection};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::{collections::HashSet, time::Duration};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateAgentArgs {
    name: String,
    task: String,
    /// Pinned permitted definition name. Required; there is no default.
    agent_template: Option<String>,
    /// Exact allowed model selection, including its effort policy.
    /// May be omitted when the template enables a default or has only one choice.
    model: Option<ModelSelection>,
}
pub struct CreateAgent {
    handle: SubagentCoordinator,
}
impl CreateAgent {
    pub fn with_handle(handle: SubagentCoordinator) -> Self {
        Self { handle }
    }
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<CreateAgentArgs>(
            "create_agent",
            "Start a named child asynchronously in this session. agent_template must name a permitted template. model must match an allowed selection including its effort policy; templates with multiple choices and no default require an explicit selection. Names must be session-unique.",
            Some(false),
        )
    }
    async fn execute_typed(
        &self,
        args: CreateAgentArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> ToolOutput {
        let handle = match require_coordinator(&self.handle, context) {
            Ok(handle) => handle,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };
        let CreateAgentArgs {
            name,
            task,
            agent_template: template,
            model,
        } = args;
        if name.trim().is_empty() || task.trim().is_empty() {
            return ToolOutput::error(prompts::text("name and task must be nonempty"));
        }
        let Some(template) = template.filter(|name| !name.trim().is_empty()) else {
            return ToolOutput::error(prompts::text(
                "agent_template is required; no default configured",
            ));
        };
        if context.definition().permitted_subagents.is_empty() {
            return ToolOutput::error(prompts::text("This agent may not create subagents"));
        }

        let caller = context.agent().id;
        let result = async {
            let _guard = handle.begin_operation()?;
            let selected = resolve_template(context, &template).await?;
            let agent = context
                .sessions()
                .create_agent_with_model(
                    context.session().id,
                    psa_api::CreateAgent {
                        subagent_parent_id: Some(caller),
                        definition_id: selected.id,
                        name,
                        interactive: false,
                        hidden: false,
                    },
                    model,
                )
                .await
                .map_err(|error| error.to_string())?;

            let submitted = context
                .sessions()
                .send_subagent_message(
                    context.session().id,
                    agent.id,
                    Some(AgentMessageSource {
                        agent_id: Some(caller),
                        name: None,
                    }),
                    crate::prompts::text(task),
                )
                .await;

            // Finish creation and initial submission within the cancellation
            // grace period instead of abandoning an already-created child.
            // The runner can still force-drop this future when grace expires.
            if cancellation.is_cancelled() {
                context
                    .sessions()
                    .interrupt(context.session().id, agent.id)
                    .await
                    .map_err(|error| {
                        format!(
                            "Agent {} ({}) created but cancellation interruption failed: {error}",
                            agent.id.0, agent.name
                        )
                    })?;
                return Err(format!(
                    "Agent creation cancelled after creating agent {} ({}); interruption requested",
                    agent.id.0, agent.name
                ));
            }

            submitted.map_err(|error| {
                format!(
                    "Agent {} created but task submission failed: {error}",
                    agent.id.0
                )
            })?;
            Ok(json!({"agent_id": agent.id, "name": agent.name, "status": "started"}))
        }
        .await;
        format_tool_output(result)
    }
}

/// Resolve a name only against the caller's pinned permitted definitions.
async fn resolve_template(
    context: &AgentContext<'_>,
    name: &str,
) -> Result<crate::agents::AgentDefinition, String> {
    let mut selected = None;
    let mut seen = HashSet::new();
    for id in &context.definition().permitted_subagents {
        if !seen.insert(*id) {
            continue;
        }
        let definition = context
            .sessions()
            .get_agent_definition(*id)
            .await
            .map_err(|error| error.to_string())?;
        if definition.name != name {
            continue;
        }
        if selected.is_some() {
            return Err(format!("Ambiguous permitted agent template: {name}"));
        }
        selected = Some(definition);
    }
    selected.ok_or_else(|| format!("Agent template not permitted: {name}"))
}

impl Tool for CreateAgent {
    fn tool_definition(&self) -> ToolDefinition {
        Self::definition()
    }
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    typed_tool_execute!(CreateAgentArgs, execute_typed);
}
#[cfg(test)]
#[path = "create_agent_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::parse_tool_arguments;
    use serde_json::json;
    #[test]
    fn typed_schema_accepts_optional_null_template_and_rejects_invalid_input() {
        for value in [
            json!({"name":"worker","task":"work"}),
            json!({"name":"worker","task":"work","agent_template":null}),
            json!({"name":"worker","task":"work","agent_template":"rust"}),
            json!({"name":"worker","task":"work","agent_template":"rust","model":"Global"}),
            json!({"name":"worker","task":"work","model":{"Fixed":{
                "model":"openrouter/vendor/model","effort":{"Fixed":"High"}
            }}}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_ok());
        }
        for value in [
            json!({"name":"worker"}),
            json!({"name":2,"task":"work"}),
            json!({"name":"worker","task":"work","extra":true}),
            json!({"name":"worker","task":"work","agent_template":false}),
            json!({"name":"worker","task":"work","model":12}),
            json!({"name":"worker","task":"work","model":"global"}),
            json!({"name":"worker","task":"work","model":{"Fixed":{
                "model":"openrouter/vendor/model"
            }}}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_err());
        }
        let parameters = serde_json::to_value(CreateAgent::definition().parameters).unwrap();
        assert_eq!(parameters["additionalProperties"], false);
    }
}
