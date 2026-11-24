use super::{format_delivery_output, require_coordinator};
use crate::{
    agents::agent_context::AgentContext,
    components::{
        bundles::subagents::{ChildTemplate, SubagentCoordinator, templates::select_template},
        cancellation::Cancellation,
        tools::{Tool, ToolOutput, typed_tool_execute},
    },
    prompts,
};
use llm_api::api::{ToolDefinition, ToolSchema};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};

const DESCRIPTION: &str = "Start a child agent that works on task asynchronously. It does not see your conversation, so task must contain everything it needs. Its final answer is delivered to you automatically as a message; there is no need to poll. Names must be unique in this session.";
const NAME_DOC: &str = "Unique name for the child; also used with send_message.";
const TASK_DOC: &str = "Complete, self-contained instructions for the child.";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateAgentArgs {
    /// Unique name for the child; also used with send_message.
    name: String,
    /// Complete, self-contained instructions for the child.
    task: String,
    /// Permitted template name; may be omitted when only one is permitted.
    agent_template: Option<String>,
    /// Allowed model choice; may be omitted when the template has a default.
    model: Option<String>,
}

pub struct CreateAgent {
    handle: SubagentCoordinator,
    templates: Arc<[ChildTemplate]>,
}

impl CreateAgent {
    pub fn new(handle: SubagentCoordinator, templates: Arc<[ChildTemplate]>) -> Self {
        Self { handle, templates }
    }

    /// Configuration-independent definition; installed instances narrow it.
    pub(crate) fn definition() -> ToolDefinition {
        ToolDefinition::for_type::<CreateAgentArgs>("create_agent", DESCRIPTION, Some(false))
    }

    /// Only expose choices that exist: a single template or model is implicit.
    fn build_definition(templates: &[ChildTemplate]) -> ToolDefinition {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "name".into(),
            json!({"type": "string", "description": NAME_DOC}),
        );
        properties.insert(
            "task".into(),
            json!({"type": "string", "description": TASK_DOC}),
        );
        let mut required = vec![json!("name"), json!("task")];

        if templates.len() > 1 {
            let names: Vec<_> = templates
                .iter()
                .map(|template| json!(template.name))
                .collect();
            properties.insert(
                "agent_template".into(),
                json!({"enum": names, "description": "Which permitted child template to start."}),
            );
            required.push(json!("agent_template"));
        }

        let mut labels: Vec<String> = Vec::new();
        for template in templates
            .iter()
            .filter(|template| template.has_model_choice())
        {
            for choice in &template.models {
                if !labels.contains(&choice.label) {
                    labels.push(choice.label.clone());
                }
            }
        }
        if !labels.is_empty() {
            properties.insert(
                "model".into(),
                json!({
                    "enum": labels,
                    "description": "Model for the child; must be allowed by its template. Omit to use the template's default.",
                }),
            );
        }

        let parameters = json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        });
        ToolDefinition {
            name: "create_agent".into(),
            description: DESCRIPTION.into(),
            parameters: ToolSchema::from(parameters),
            strict: Some(false),
        }
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
            agent_template,
            model,
        } = args;
        if name.trim().is_empty() || task.trim().is_empty() {
            return ToolOutput::error(prompts::text("name and task must be nonempty"));
        }
        let template_name = agent_template
            .as_deref()
            .filter(|name| !name.trim().is_empty());
        let selected = select_template(&self.templates, template_name).and_then(|template| {
            Ok((
                template.definition_id,
                template.resolve_model(model.as_deref())?,
            ))
        });
        let (definition_id, model) = match selected {
            Ok(selected) => selected,
            Err(error) => return ToolOutput::error(prompts::text(error)),
        };

        let caller = context.agent().id;
        let result = async {
            let _guard = handle.begin_operation()?;
            let agent = context
                .runtime()
                .create_agent_with_model(
                    context.session().id,
                    crate::agents::NewAgent {
                        subagent_parent_id: Some(caller),
                        definition_id,
                        name,
                        interactive: false,
                        hidden: false,
                    },
                    model,
                )
                .await
                .map_err(|error| error.to_string())?;
            let submitted = context
                .runtime()
                .send_subagent_message(
                    context.session().id,
                    &agent,
                    context.agent(),
                    crate::prompts::text(task),
                )
                .await;
            // Finish creation and initial submission within the cancellation
            // grace period instead of abandoning an already-created child.
            // The runner can still force-drop this future when grace expires.
            if cancellation.is_cancelled() {
                context
                    .runtime()
                    .interrupt(context.session().id, agent.id)
                    .await
                    .map_err(|error| {
                        format!(
                            "Agent \"{}\" was created but interrupting it after cancellation failed: {error}",
                            agent.name
                        )
                    })?;
                return Err(format!(
                    "Cancelled after creating agent \"{}\"; it has been interrupted",
                    agent.name
                ));
            }
            let link = submitted.map_err(|error| {
                format!(
                    "Agent \"{}\" was created but its task could not be sent: {error}",
                    agent.name
                )
            })?;
            handle.note_delivery(agent.id);
            Ok((
                format!(
                    "Started agent \"{}\". Its final answer will arrive as a message.",
                    agent.name
                ),
                link,
            ))
        }
        .await;
        format_delivery_output(result)
    }
}

impl Tool for CreateAgent {
    fn tool_definition(&self) -> ToolDefinition {
        Self::build_definition(&self.templates)
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }

    typed_tool_execute!(CreateAgentArgs, execute_typed);
}

#[cfg(test)]
#[path = "../../../tests/components/tools/subagents/create_agent/cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::bundles::subagents::templates::{ModelChoice, model_label};
    use llm_api::api::parse_tool_arguments;
    use psa::api::{AgentDefinitionId, ModelSelection};
    use serde_json::Value;

    fn template(name: &str, models: &[ModelSelection], default: bool) -> ChildTemplate {
        ChildTemplate {
            definition_id: AgentDefinitionId(1),
            name: name.into(),
            description: String::new(),
            models: models
                .iter()
                .map(|selection| ModelChoice {
                    label: model_label(selection),
                    selection: selection.clone(),
                })
                .collect(),
            default,
        }
    }

    fn schema(templates: &[ChildTemplate]) -> Value {
        serde_json::to_value(CreateAgent::build_definition(templates).parameters).unwrap()
    }

    #[test]
    fn typed_arguments_accept_optional_choices_and_reject_invalid_input() {
        for value in [
            json!({"name":"worker","task":"work"}),
            json!({"name":"worker","task":"work","agent_template":null}),
            json!({"name":"worker","task":"work","agent_template":"rust","model":"parent"}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_ok());
        }
        for value in [
            json!({"name":"worker"}),
            json!({"name":2,"task":"work"}),
            json!({"name":"worker","task":"work","extra":true}),
            json!({"name":"worker","task":"work","agent_template":false}),
            json!({"name":"worker","task":"work","model":{"Fixed":{}}}),
        ] {
            assert!(parse_tool_arguments::<CreateAgentArgs>(value).is_err());
        }
    }

    #[test]
    fn single_template_and_model_are_not_exposed_as_choices() {
        let schema = schema(&[template("worker", &[ModelSelection::Parent], true)]);
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["name", "task"]));
        assert!(schema["properties"].get("agent_template").is_none());
        assert!(schema["properties"].get("model").is_none());
        assert!(!schema.to_string().contains("Global"));
    }

    #[test]
    fn multiple_templates_and_models_become_enums() {
        let schema = schema(&[
            template(
                "worker",
                &[ModelSelection::Parent, ModelSelection::Global],
                false,
            ),
            template("reviewer", &[ModelSelection::Parent], true),
        ]);
        assert_eq!(
            schema["properties"]["agent_template"]["enum"],
            json!(["worker", "reviewer"])
        );
        assert_eq!(
            schema["required"],
            json!(["name", "task", "agent_template"])
        );
        assert_eq!(
            schema["properties"]["model"]["enum"],
            json!(["parent", "session"])
        );
    }
}
