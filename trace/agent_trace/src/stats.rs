//! Aggregate counts over agent logs.

use crate::model::{AgentTrace, EventKind, FinishOutcome};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentStats {
    pub events: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub agent_messages: usize,
    pub finishes: usize,
    pub aborted_finishes: usize,
    pub compactions: usize,
    pub usage: Usage,
    pub tools: BTreeMap<String, ToolUsage>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub inferences: usize,
    pub failed_inferences: usize,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    /// Sum of reported costs; `None` when no inference reported one.
    pub cost_usd: Option<f64>,
    pub inference_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolUsage {
    pub calls: usize,
    pub errors: usize,
    /// Results without a matching call are counted under the result only.
    pub results: usize,
    pub total_ms: u64,
}

impl AgentStats {
    pub fn compute(agent: &AgentTrace) -> Self {
        let mut stats = Self {
            events: agent.events.len(),
            ..Self::default()
        };
        for event in &agent.events {
            match &event.kind {
                EventKind::User { .. } => stats.user_messages += 1,
                EventKind::Assistant { .. } => stats.assistant_messages += 1,
                EventKind::AgentMessage { .. } => stats.agent_messages += 1,
                EventKind::Compaction { .. } => stats.compactions += 1,
                EventKind::AgentFinish { outcome, .. } => {
                    stats.finishes += 1;
                    if *outcome != FinishOutcome::Completed {
                        stats.aborted_finishes += 1;
                    }
                }
                EventKind::ToolCall { name, result, .. } => {
                    let usage = stats.tools.entry(name.clone()).or_default();
                    usage.calls += 1;
                    let Some(result) = result else {
                        continue;
                    };
                    usage.results += 1;
                    usage.errors += usize::from(result.is_error);
                    usage.total_ms += result.duration_ms.unwrap_or(0);
                }
                EventKind::Inference(inference) => {
                    let usage = &mut stats.usage;
                    usage.inferences += 1;
                    usage.failed_inferences += usize::from(inference.error.is_some());
                    usage.input_tokens += inference.input_tokens.unwrap_or(0);
                    usage.cached_input_tokens += inference.cached_input_tokens.unwrap_or(0);
                    usage.cache_write_tokens += inference.cache_write_tokens.unwrap_or(0);
                    usage.output_tokens += inference.output_tokens.unwrap_or(0);
                    usage.reasoning_tokens += inference.reasoning_tokens.unwrap_or(0);
                    usage.inference_ms += inference.duration_ms.unwrap_or(0);
                    if let Some(cost) = inference.cost_usd {
                        usage.cost_usd = Some(usage.cost_usd.unwrap_or(0.0) + cost);
                    }
                }
                EventKind::System { .. }
                | EventKind::Thinking { .. }
                | EventKind::Context { .. }
                | EventKind::ModelChange { .. } => {}
            }
        }
        stats
    }

    /// Combine per-agent statistics into session totals.
    pub fn merge(&mut self, other: &Self) {
        self.events += other.events;
        self.user_messages += other.user_messages;
        self.assistant_messages += other.assistant_messages;
        self.agent_messages += other.agent_messages;
        self.finishes += other.finishes;
        self.aborted_finishes += other.aborted_finishes;
        self.compactions += other.compactions;

        let usage = &mut self.usage;
        usage.inferences += other.usage.inferences;
        usage.failed_inferences += other.usage.failed_inferences;
        usage.input_tokens += other.usage.input_tokens;
        usage.cached_input_tokens += other.usage.cached_input_tokens;
        usage.cache_write_tokens += other.usage.cache_write_tokens;
        usage.output_tokens += other.usage.output_tokens;
        usage.reasoning_tokens += other.usage.reasoning_tokens;
        usage.inference_ms += other.usage.inference_ms;
        usage.cost_usd = match (usage.cost_usd, other.usage.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };

        for (name, tool) in &other.tools {
            let total = self.tools.entry(name.clone()).or_default();
            total.calls += tool.calls;
            total.errors += tool.errors;
            total.results += tool.results;
            total.total_ms += tool.total_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentMeta, Event, Inference, ToolResult};
    use serde_json::json;

    #[test]
    fn attributes_results_to_called_tools() {
        let event = |kind| Event::new(None, kind);
        let agent = AgentTrace {
            meta: AgentMeta::new("main"),
            events: vec![
                event(EventKind::ToolCall {
                    name: "bash".into(),
                    args: json!({}),
                    result: Some(ToolResult {
                        is_error: true,
                        duration_ms: Some(30),
                        ..Default::default()
                    }),
                }),
                event(EventKind::ToolCall {
                    name: "bash".into(),
                    args: json!({}),
                    result: None,
                }),
                event(EventKind::Inference(Inference {
                    input_tokens: Some(100),
                    output_tokens: Some(5),
                    cost_usd: Some(0.5),
                    ..Default::default()
                })),
            ],
        };

        let stats = AgentStats::compute(&agent);
        let bash = &stats.tools["bash"];
        assert_eq!(
            (bash.calls, bash.results, bash.errors, bash.total_ms),
            (2, 1, 1, 30)
        );
        assert_eq!(stats.usage.input_tokens, 100);

        let mut total = AgentStats::default();
        total.merge(&stats);
        total.merge(&stats);
        assert_eq!(total.usage.cost_usd, Some(1.0));
        assert_eq!(total.tools["bash"].calls, 4);
    }
}
