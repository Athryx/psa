//! Exposes an agent's installed codemode functions and checkpoints to the engine.

use super::{CodeModeError, CodeModeFunction};
use crate::{agents::agent_context::AgentContext, components::cancellation::Cancellation};
use psa_codemode::{
    CodeModeHost, HostFunction,
    result_classes::{self, ResultWrapper},
};
use psa_store::{PrivateEvent, PrivateEventKind};
use serde_json::Value;

pub(crate) struct AgentCodeModeHost<'c, 'a> {
    context: &'c AgentContext<'a>,
}

impl<'c, 'a> AgentCodeModeHost<'c, 'a> {
    pub(crate) fn new(context: &'c AgentContext<'a>) -> Self {
        Self { context }
    }
}

impl CodeModeHost for AgentCodeModeHost<'_, '_> {
    async fn load_snapshot(&self) -> Result<Option<Vec<u8>>, String> {
        let event = self
            .context
            .load_latest_private_event(PrivateEventKind::CodeModeSnapshot)
            .await
            .map_err(|error| error.to_string())?;
        Ok(event.and_then(|event| match event {
            PrivateEvent::CodeModeSnapshot(bytes) => Some(bytes),
            _ => None,
        }))
    }

    fn prelude(&self) -> Option<String> {
        let results: Vec<_> = self
            .context
            .codemode_functions()
            .iter()
            .filter_map(|entry| {
                let result = entry.function.result_type()?;
                Some((entry.function.definition().name, result))
            })
            .collect();
        result_classes::build_result_prelude(results.iter().map(|(function, result)| {
            ResultWrapper {
                function,
                class: &result.name,
                schema: result.schema.as_value(),
            }
        }))
    }

    fn functions(&self) -> Vec<HostFunction> {
        self.context
            .codemode_functions()
            .iter()
            .map(|entry| entry.function.host_function())
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancellation: Cancellation,
    ) -> Result<Value, CodeModeError> {
        let function = self
            .context
            .codemode_functions()
            .iter()
            .map(|entry| &entry.function)
            .find(|function| function.definition().name == name)
            .ok_or_else(|| CodeModeError::runtime(format!("name '{name}' is not defined")))?;
        let mut child = self
            .context
            .child_invocation()
            .ok_or_else(|| CodeModeError::runtime("Nested invocation unavailable"))?;
        function.execute(arguments, &mut child, cancellation).await
    }
}
