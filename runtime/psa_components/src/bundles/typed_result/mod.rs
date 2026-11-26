use super::{Bundle, CleanupError, HookStage, InstallError, Installer};
use crate::{hooks::RequireResult, tools::RecordResult};
use llm_api::api::ToolSchema;
use psa_agents::Agent;
use psa_store::Session;
use std::sync::Arc;

mod schema;
pub use schema::{ResultSchema, ResultSchemaError};

/// Immutable configuration shared by the tool and its completion policy.
/// Retry state is reconstructed from the invocation's persisted history.
pub(crate) struct TypedResultState {
    pub schema: ToolSchema,
    pub compiled: ResultSchema,
    pub max_retries: u32,
}

pub struct TypedResultBundle {
    state: Arc<TypedResultState>,
}

impl TypedResultBundle {
    pub fn new(schema: ToolSchema, max_retries: u32) -> Result<Self, InstallError> {
        let compiled = ResultSchema::compile(&schema)
            .map_err(|error| InstallError::new(format!("Invalid TypedResult schema: {error}")))?;
        Ok(Self {
            state: Arc::new(TypedResultState {
                schema,
                compiled,
                max_retries,
            }),
        })
    }
}

impl Bundle for TypedResultBundle {
    fn label(&self) -> String {
        "TypedResult".into()
    }

    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_tool(RecordResult::with_state(self.state.clone()))?;
        installer.add_hook(
            HookStage::AfterRequest,
            RequireResult::with_state(self.state.clone()),
        );
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn construction_compiles_only_supported_object_schemas() {
        for schema in [
            json!({"type":"object"}),
            json!({
                "type":"object",
                "properties":{"value":{"$ref":"#/$defs/number"}},
                "$defs":{"number":{"type":"integer"}}
            }),
        ] {
            assert!(TypedResultBundle::new(schema.into(), 3).is_ok());
        }
        for schema in [
            json!({"type":"string"}),
            json!({"type":"object", "properties":{"value":{"$ref":"https://invalid.test/schema"}}}),
        ] {
            assert!(TypedResultBundle::new(schema.into(), 3).is_err());
        }
    }
}
