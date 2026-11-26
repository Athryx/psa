//! Completion-policy validation for agent configuration boundaries.

use super::typed_result::{ResultSchema, ResultSchemaError};
use psa::api::{BundleConfig, CompletionConfigurationError, validate_completion_modes};

#[derive(Debug, thiserror::Error)]
pub enum CompletionConfigError {
    #[error(transparent)]
    Modes(#[from] CompletionConfigurationError),
    #[error("invalid TypedResult schema: {0}")]
    InvalidSchema(#[from] ResultSchemaError),
}

/// Check the selected completion modes and compile any TypedResult schema, so
/// invalid configurations are rejected before they are saved.
pub fn validate_completion_config(bundles: &[BundleConfig]) -> Result<(), CompletionConfigError> {
    validate_completion_modes(bundles)?;

    for bundle in bundles {
        if let BundleConfig::TypedResult { schema, .. } = bundle {
            ResultSchema::compile(schema)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::ToolSchema;
    use serde_json::json;

    fn typed(schema: serde_json::Value) -> BundleConfig {
        BundleConfig::TypedResult {
            schema: ToolSchema::from(schema),
            max_retries: 0,
        }
    }

    #[test]
    fn rejects_invalid_schemas_and_incompatible_modes() {
        assert!(validate_completion_config(&[typed(json!({"type":"object"}))]).is_ok());
        assert!(matches!(
            validate_completion_config(&[typed(json!({"type":"string"}))]),
            Err(CompletionConfigError::InvalidSchema(_))
        ));
        assert!(matches!(
            validate_completion_config(&[
                typed(json!({"type":"object"})),
                BundleConfig::StopWithoutTools
            ]),
            Err(CompletionConfigError::Modes(_))
        ));
    }
}
