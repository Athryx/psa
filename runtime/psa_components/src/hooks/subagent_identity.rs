//! Identity is emitted at creation; forks preserve their inherited prompt.
use super::{Hook, HookError};
use crate::bundles::subagents::ChildTemplate;
use psa::api::events::{EventType, Message};
use psa_agents::{
    agent_context::AgentContext, cancellation::Cancellation, supervisor::RuntimeError,
};
use psa_store::NewEvent;
use std::{sync::Arc, time::Duration};

pub struct SubagentIdentity {
    templates: Arc<[ChildTemplate]>,
}
impl SubagentIdentity {
    pub fn new(templates: Arc<[ChildTemplate]>) -> Self {
        Self { templates }
    }

    async fn build_identity(&self, context: &AgentContext<'_>) -> Result<String, String> {
        let agent = context.agent();
        let parent = match agent.subagent_parent_id {
            Some(id) => Some(
                context
                    .runtime()
                    .get_agent(context.session().id, id)
                    .await
                    .map_err(|error| error.to_string())?
                    .name,
            ),
            None => None,
        };
        Ok(format_identity(
            &agent.name,
            parent.as_deref(),
            &self.templates,
        ))
    }
}

fn format_identity(name: &str, parent: Option<&str>, templates: &[ChildTemplate]) -> String {
    let mut lines = vec![match parent {
        Some(parent) => format!(
            "You are agent \"{name}\", started by \"{parent}\" to complete its task. Your final answer is sent to \"{parent}\" automatically."
        ),
        None => format!("You are agent \"{name}\"."),
    }];
    if !templates.is_empty() {
        lines.push("Child agents you can start with create_agent:".into());
        lines.extend(templates.iter().map(ChildTemplate::describe));
        lines.push("Children run asynchronously and do not see your conversation, so give each a self-contained task. Their final answers arrive as messages from them.".into());
    }
    lines.push("Use send_message to message another agent in this session and wait_message to wait for incoming messages.".into());
    format!("<agent_identity>\n{}\n</agent_identity>", lines.join("\n"))
}

impl Hook for SubagentIdentity {
    fn name(&self) -> &'static str {
        "SubagentIdentity"
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(2)
    }
    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        if context.agent().forked_from.is_some() {
            return Ok(());
        }

        let identity = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = self.build_identity(context) => result.map_err(RuntimeError::Invalid)?,
        };

        let events = [NewEvent::new(EventType::Message(Message::system(
            identity,
            "Agent identity",
        )))];
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = context.save_events(&events) => { result?; },
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundles::subagents::templates::ModelChoice;
    use psa::api::{AgentDefinitionId, ModelSelection};

    #[test]
    fn identity_uses_names_and_plain_template_descriptions() {
        let templates = [ChildTemplate {
            definition_id: AgentDefinitionId(7),
            name: "subagent".into(),
            description: "Delegated worker".into(),
            models: vec![ModelChoice {
                label: "parent".into(),
                selection: ModelSelection::Parent,
            }],
            default: true,
        }];
        let text = format_identity("default", None, &templates);
        assert_eq!(
            text,
            concat!(
                "<agent_identity>\n",
                "You are agent \"default\".\n",
                "Child agents you can start with create_agent:\n",
                "- subagent: Delegated worker. Model: same as yours.\n",
                "Children run asynchronously and do not see your conversation, so give each a self-contained task. Their final answers arrive as messages from them.\n",
                "Use send_message to message another agent in this session and wait_message to wait for incoming messages.\n",
                "</agent_identity>"
            )
        );
        assert!(!text.contains('7'));
        assert!(!text.contains('{'));
    }

    #[test]
    fn child_identity_names_its_parent_and_omits_empty_templates() {
        let text = format_identity("worker", Some("default"), &[]);
        assert!(text.contains("started by \"default\""));
        assert!(!text.contains("create_agent"));
        assert!(!text.contains("Agent ID"));
    }
}
