//! Pi-style context compaction, with inference delegated to ordinary hidden agents.
mod prepare;
mod prompts;
#[cfg(test)]
mod tests;

use super::{CommandHook, Hook, HookError};
use crate::agents::NewAgent;
use crate::db::{CreateAgentDefinition, EventRange, NewEvent};
use crate::{
    agents::{AgentState, agent_context::AgentContext, supervisor::RuntimeError},
    components::cancellation::Cancellation,
};
use llm_api::{
    api::{ContentBlock, Outcome, StopReason},
    providers::ModelConfig,
};
use psa::api::{
    AgentCommand, AgentDefinitionId, AgentResult, BundleConfig, CommandOutcome, CommandResult,
    events::{Compaction, CompactionSummary, Event, EventType, Log, MessageType},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// Summarizer bookkeeping shared by the automatic and manual installations, so
/// both reuse one definition and never pick the same summarizer agent name.
#[derive(Default)]
struct Summarizers {
    counter: Option<u64>,
    definition: Option<(ModelConfig, AgentDefinitionId)>,
}

/// Installed by the Compaction bundle both before requests and as a command hook.
#[derive(Clone)]
pub struct CompactionHook {
    reserve_tokens: u64,
    keep_recent_tokens: u64,
    context_window: Option<u64>,
    summarizers: Arc<Mutex<Summarizers>>,
}

impl CompactionHook {
    pub fn new(reserve_tokens: u64, keep_recent_tokens: u64, context_window: Option<u64>) -> Self {
        Self {
            reserve_tokens,
            keep_recent_tokens,
            context_window,
            summarizers: Arc::default(),
        }
    }

    /// Compacts a running agent only once its context nears the model's window.
    async fn compact_if_needed(
        &mut self,
        context: &mut AgentContext<'_>,
        cancellation: &mut Cancellation,
    ) -> Result<(), HookError> {
        if cancellation.is_cancelled() || context.agent().state != AgentState::Running {
            return Ok(());
        }
        let Some(model) = context.projection().model_config().cloned() else {
            return Ok(());
        };
        let window = match self.context_window {
            Some(window) => Some(window),
            None => match context.runtime().model_catalog() {
                Some(catalog) => catalog
                    .model_info(&model.model)
                    .await
                    .map_err(|error| HookError::Compaction(error.to_string()))?
                    .and_then(|info| info.context_window),
                None => None,
            },
        };
        let Some(window) = window.filter(|window| *window > 0) else {
            return Ok(());
        };
        let events = context.load_context_window().await?;
        if prepare::estimate_context_tokens(&events) <= window.saturating_sub(self.reserve_tokens) {
            return Ok(());
        }
        let Some(preparation) = prepare::prepare(&events, self.keep_recent_tokens) else {
            return Ok(());
        };

        self.replace_context(context, &events, &model, preparation, cancellation)
            .await
    }

    /// Compacts on request in any agent state, reporting no-ops in the transcript.
    /// The recent-context budget is ignored: everything before the latest entry is
    /// summarized, so the request compacts whenever there is more than one entry.
    async fn compact_now(
        &mut self,
        context: &mut AgentContext<'_>,
        instructions: Option<&str>,
        cancellation: &mut Cancellation,
    ) -> Result<(), HookError> {
        let Some(model) = context.projection().model_config().cloned() else {
            return save_log(
                context,
                false,
                "Nothing to compact: the agent has no model yet.",
            )
            .await;
        };
        let events = context.load_context_window().await?;
        let Some(mut preparation) = prepare::prepare(&events, 0) else {
            return save_log(
                context,
                false,
                "Nothing to compact: there is no earlier conversation to summarize.",
            )
            .await;
        };

        if let Some(instructions) = instructions.map(str::trim).filter(|text| !text.is_empty()) {
            for prompt in [
                &mut preparation.history_prompt,
                &mut preparation.turn_prefix_prompt,
            ]
            .into_iter()
            .flatten()
            {
                prompt.push_str(&prompts::additional_instructions(instructions));
            }
        }
        self.replace_context(context, &events, &model, preparation, cancellation)
            .await
    }

    async fn replace_context(
        &mut self,
        context: &mut AgentContext<'_>,
        events: &[Event],
        model: &ModelConfig,
        preparation: prepare::Preparation,
        cancellation: &mut Cancellation,
    ) -> Result<(), HookError> {
        let history = match &preparation.history_prompt {
            Some(prompt) => Some(self.summarize(context, model, prompt, cancellation).await?),
            None => None,
        };
        let prefix = match &preparation.turn_prefix_prompt {
            Some(prompt) => Some(self.summarize(context, model, prompt, cancellation).await?),
            None => None,
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let summary = preparation.finish_summary(history.as_deref(), prefix.as_deref());
        tracing::debug!(
            agent = context.agent().id.0,
            tokens_before = preparation.tokens_before,
            "compacting context"
        );
        let replacement = build_replacement(events, model, preparation, summary);
        context.save_events(&replacement).await?;
        Ok(())
    }

    async fn summarize(
        &mut self,
        context: &AgentContext<'_>,
        model: &ModelConfig,
        prompt: &str,
        cancellation: &mut Cancellation,
    ) -> Result<String, HookError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        let sessions = context.runtime();
        let session = context.session().id;
        let mut summarizers = self.summarizers.lock().await;
        let definition_id = match &summarizers.definition {
            Some((configured, id)) if configured == model => *id,
            _ => {
                let definition = sessions
                    .create_agent_definition(CreateAgentDefinition {
                        name: "_internal_compaction".into(),
                        model: model.clone().into(),
                        bundles: vec![
                            BundleConfig::Startup {
                                base_prompt: false,
                                project_context: false,
                                cwd: false,
                                skills: false,
                            },
                            BundleConfig::ExtraSystemPrompt {
                                prompt: prompts::SUMMARIZATION_SYSTEM_PROMPT.into(),
                            },
                            BundleConfig::StopWithoutTools,
                        ],
                        ..Default::default()
                    })
                    .await?;
                summarizers.definition = Some((model.clone(), definition.id));
                definition.id
            }
        };
        let prefix = format!("_internal_{}_compaction", context.agent().name);
        if summarizers.counter.is_none() {
            summarizers.counter = Some(
                sessions
                    .list_agents(session)
                    .await?
                    .iter()
                    .filter_map(|agent| agent.name.strip_prefix(&prefix)?.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0),
            );
        }
        let count = summarizers
            .counter
            .unwrap()
            .checked_add(1)
            .ok_or_else(|| HookError::Compaction("summarizer name counter exhausted".into()))?;
        summarizers.counter = Some(count);
        drop(summarizers);
        let agent = sessions
            .create_agent_with_model(
                session,
                NewAgent {
                    definition_id,
                    name: format!("{prefix}{count}"),
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: true,
                },
                None,
            )
            .await?;

        // Await registration/submission even on cancellation, then stop that exact
        // agent. Dropping a queued submission would not retract it from the manager.
        let result = async {
            if cancellation.is_cancelled() {
                return Err(RuntimeError::Cancelled.into());
            }
            sessions
                .steer(session, agent.id, crate::prompts::text(prompt))
                .await?;
            let result = sessions
                .await_result(session, agent.id, None, cancellation)
                .await?;
            let events = sessions
                .load_agent_events(session, agent.id, EventRange::default())
                .await?;
            require_summary(result.value, &events)
        }
        .await;
        if result.is_err() {
            // Preserve the original context on every failure, including cancellation.
            sessions.interrupt(session, agent.id).await?;
        }
        result
    }
}

fn require_summary(result: AgentResult, events: &[Event]) -> Result<String, HookError> {
    let complete = events.iter().rev().find_map(|event| match &event.kind {
        EventType::InferenceResultEnd(end) => Some(matches!(
            end.outcome,
            Outcome::Completed { ref reason } if *reason != StopReason::TokenLimit
        )),
        _ => None,
    });
    let invalid_output = events.iter().any(|event| match &event.kind {
        EventType::ToolCall(_) => true,
        EventType::Message(message) => message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Refusal(_))),
        _ => false,
    });
    match result {
        AgentResult::Text(text)
            if !text.trim().is_empty() && complete == Some(true) && !invalid_output =>
        {
            Ok(text)
        }
        _ => Err(HookError::Compaction(
            "summarizer did not produce a complete text summary".into(),
        )),
    }
}

fn build_replacement(
    events: &[Event],
    model: &ModelConfig,
    preparation: prepare::Preparation,
    summary: String,
) -> Vec<NewEvent> {
    let mut replacement = vec![
        NewEvent::new(EventType::NewContext),
        NewEvent::new(EventType::Model(
            preparation
                .retained_model
                .clone()
                .unwrap_or_else(|| model.clone()),
        )),
    ];
    replacement.extend(events.iter().filter_map(|event| match &event.kind {
        EventType::Message(message) if message.message_type == MessageType::System => {
            Some(NewEvent::new(event.kind.clone()))
        }
        EventType::ToolDefinition(_) => Some(NewEvent::new(event.kind.clone())),
        _ => None,
    }));
    replacement.push(NewEvent::new(EventType::Compaction(Compaction {
        summary: CompactionSummary::Text(summary),
        // Summary inference is already billed on its own agent, not twice here.
        usage: Default::default(),
    })));
    replacement.extend(preparation.retained_events);
    replacement
}

impl Hook for CompactionHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        match self.compact_if_needed(context, &mut cancellation).await {
            Err(HookError::Runtime(RuntimeError::Cancelled)) if cancellation.is_cancelled() => {
                Ok(())
            }
            result => result,
        }
    }
}

impl CommandHook for CompactionHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn handle_command(
        &mut self,
        context: &mut AgentContext<'_>,
        command: &AgentCommand,
        mut cancellation: Cancellation,
    ) -> Result<Option<CommandResult>, HookError> {
        // New command variants must decide here whether compaction ignores them.
        let instructions = match command {
            AgentCommand::Compact { instructions } => instructions.as_deref(),
            AgentCommand::Goal(_) => return Ok(None),
        };

        // A manual request reports failures in its command result rather than
        // failing the worker; persistence failures still propagate.
        let (outcome, output) = match self
            .compact_now(context, instructions, &mut cancellation)
            .await
        {
            Ok(()) => (CommandOutcome::Succeeded, None),
            Err(HookError::Runtime(RuntimeError::Cancelled)) if cancellation.is_cancelled() => {
                (CommandOutcome::Interrupted, None)
            }
            Err(error @ (HookError::Database(_) | HookError::Agent(_))) => return Err(error),
            Err(error) => (
                CommandOutcome::Failed,
                Some(format!("Compaction failed: {error}")),
            ),
        };
        Ok(Some(CommandResult { outcome, output }))
    }
}

async fn save_log(
    context: &mut AgentContext<'_>,
    error: bool,
    message: &str,
) -> Result<(), HookError> {
    context
        .save_events(&[NewEvent::new(EventType::Log(Log {
            error,
            message: message.into(),
        }))])
        .await?;
    Ok(())
}
