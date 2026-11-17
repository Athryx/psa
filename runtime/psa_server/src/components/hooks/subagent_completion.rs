//! Bundle policy invoked once at a logical run's end, independently of workers.
use crate::{
    agents::{
        lifecycle::{AgentLifecycleEvent, AgentRunOutcome},
        sessions::Sessions,
    },
    prompts,
};
use psa_api::{AgentRequest, BundleId};

pub(crate) struct SubagentCompletion;

impl SubagentCompletion {
    pub(crate) async fn run(&self, sessions: &Sessions, event: AgentLifecycleEvent) {
        if let Err(error) = self.send_report(sessions, event).await {
            tracing::debug!(%error, "subagent completion report not delivered");
        }
    }

    async fn send_report(
        &self,
        sessions: &Sessions,
        event: AgentLifecycleEvent,
    ) -> Result<(), String> {
        let AgentLifecycleEvent::Finished {
            session_id,
            agent_id,
            outcome,
            final_text,
            ..
        } = event
        else {
            return Ok(());
        };
        let agent = sessions
            .get_agent(session_id, agent_id)
            .await
            .map_err(|error| error.to_string())?;
        let Some(parent) = agent.subagent_parent_id else {
            return Ok(());
        };
        let parent_agent = sessions
            .get_agent(session_id, parent)
            .await
            .map_err(|error| error.to_string())?;
        let definition = sessions
            .get_agent_definition(parent_agent.definition_id)
            .await
            .map_err(|error| error.to_string())?;
        if !definition
            .bundles
            .iter()
            .any(|config| config.id() == BundleId::Subagents)
        {
            return Ok(());
        }

        let report = match outcome {
            AgentRunOutcome::Completed => final_text
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "Completed without a text answer.".into()),
            AgentRunOutcome::Interrupted => "Interrupted before finishing.".into(),
            AgentRunOutcome::Failed { message } => format!("Failed: {message}"),
        };

        sessions
            .steer_if_not_interrupted(
                session_id,
                parent,
                AgentRequest::Message {
                    delivery: psa_api::MessageDelivery::Steer,
                    source: Some(agent_id),
                    content: prompts::text(prompts::truncate_middle(&report, 30_000).into_owned()),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}
