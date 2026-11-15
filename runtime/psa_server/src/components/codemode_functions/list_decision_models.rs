//! `list_decision_models`: the decision models this agent may call right now.

use std::time::Duration;

use llm_api::api::ToolSchema;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    CodeModeFunction, CodeModeFunctionDefinition, CodeModeFunctionResult, typed_codemode_execute,
};
use crate::{
    agents::agent_context::AgentContext,
    components::{bundles::decision_models::DecisionAccess, cancellation::Cancellation},
};

pub struct ListDecisionModels {
    access: DecisionAccess,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDecisionModelsArgs {}

impl ListDecisionModels {
    pub(crate) fn new(access: DecisionAccess) -> Self {
        Self { access }
    }

    async fn execute_typed(
        &self,
        _arguments: ListDecisionModelsArgs,
        context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> CodeModeFunctionResult {
        let mut entries = Vec::new();
        for model in self.access.policy.legal_models(context.session()) {
            // Models without credentials or outside the catalog's decision
            // models cannot be called, so they are not offered.
            if !self.access.handle.is_available(&model) {
                continue;
            }
            let Ok(info) = self.access.handle.describe(&model).await else {
                continue;
            };
            entries.push(match info {
                Some(info) => json!({
                    "model": model.slug(),
                    "name": info.name,
                    "context_window": info.context_window,
                    "input_modalities": info.input_modalities,
                    "input_cost_per_million_tokens": info.cost.rates.input,
                }),
                None => json!({"model": model.slug()}),
            });
        }
        Ok(Value::Array(entries))
    }
}

impl CodeModeFunction for ListDecisionModels {
    fn definition(&self) -> CodeModeFunctionDefinition {
        CodeModeFunctionDefinition {
            name: "list_decision_models".into(),
            description: "List the decision models you may pass to decide(): each has model (the name to pass), name, context_window in tokens, input_modalities and input_cost_per_million_tokens. Returns an empty list when none are usable. Not async; do not await.".into(),
            parameters: ToolSchema::for_type::<ListDecisionModelsArgs>(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_codemode_execute!(ListDecisionModelsArgs, execute_typed);
}
