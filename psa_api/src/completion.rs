//! Shared configuration validation for mutually exclusive agent completion policies.

use crate::{BundleConfig, HookConfig, HookConfigs};
use llm_api::api::SchemaValidationError;

/// A configuration cannot safely establish its selected completion policy.
#[derive(Debug, thiserror::Error)]
pub enum CompletionConfigurationError {
    /// More than one distinct completion policy was selected, including defaults.
    #[error("StopWithoutTools, Goal, and TypedResult completion modes are mutually exclusive")]
    IncompatibleModes,
    /// A terminal-response hook was assigned to a nonterminal lifecycle stage.
    #[error("StopWithoutTools must be configured in after_request")]
    InvalidStopperStage,
    /// Context replacement must happen between completed tool batches and inference.
    #[error("Compaction must be configured in before_request")]
    InvalidCompactionStage,
    /// The configured typed-result schema cannot be compiled safely.
    #[error("invalid typed-result schema: {0}")]
    InvalidSchema(#[from] SchemaValidationError),
}

/// Validate completion-policy selection after applying server defaults.
///
/// Goal and TypedResult are incompatible with each other and with StopWithoutTools,
/// even when a bundle's installation policy could skip a particular agent.
/// Repeated copies of the same hook do not select a second completion mode.
/// Runtime bundle construction separately compiles and retains typed schemas.
pub fn validate_completion_modes(
    bundles: &[BundleConfig],
    hooks: &HookConfigs,
) -> Result<(), CompletionConfigurationError> {
    if hooks
        .agent_create
        .iter()
        .chain(&hooks.session_update)
        .chain(&hooks.after_request)
        .any(|hook| matches!(hook, HookConfig::Compaction { .. }))
    {
        return Err(CompletionConfigurationError::InvalidCompactionStage);
    }

    let is_stopper = |hook: &HookConfig| matches!(hook, HookConfig::StopWithoutTools);
    if hooks.agent_create.iter().any(is_stopper)
        || hooks.session_update.iter().any(is_stopper)
        || hooks.before_request.iter().any(is_stopper)
    {
        return Err(CompletionConfigurationError::InvalidStopperStage);
    }

    let ordinary = hooks.after_request.iter().any(is_stopper);
    let goal = bundles
        .iter()
        .any(|bundle| matches!(bundle, BundleConfig::Goal { .. }));
    let typed = bundles
        .iter()
        .any(|bundle| matches!(bundle, BundleConfig::TypedResult { .. }));
    if u8::from(ordinary) + u8::from(goal) + u8::from(typed) > 1 {
        return Err(CompletionConfigurationError::IncompatibleModes);
    }

    Ok(())
}

/// Validate effective completion policy and compile its schema offline.
/// Use this at configuration boundaries so invalid schemas fail before persistence.
pub fn validate_completion_config(
    bundles: &[BundleConfig],
    hooks: &HookConfigs,
) -> Result<(), CompletionConfigurationError> {
    validate_completion_modes(bundles, hooks)?;
    for bundle in bundles {
        if let BundleConfig::TypedResult { schema, .. } = bundle {
            schema.compile()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::api::ToolSchema;
    use serde_json::json;

    fn typed() -> BundleConfig {
        BundleConfig::TypedResult {
            schema: ToolSchema::from(json!({"type":"object"})),
            max_retries: 3,
        }
    }

    #[test]
    fn rejects_incompatible_modes_including_skippable_goal_policy() {
        let ordinary = HookConfigs {
            after_request: vec![HookConfig::StopWithoutTools],
            ..Default::default()
        };
        let goal = BundleConfig::Goal {
            interactive_only: true,
        };
        assert!(validate_completion_config(&[], &ordinary).is_ok());
        assert!(validate_completion_config(&[goal.clone()], &HookConfigs::default()).is_ok());
        assert!(validate_completion_config(&[typed()], &HookConfigs::default()).is_ok());
        assert!(validate_completion_config(&[goal.clone()], &ordinary).is_err());
        assert!(validate_completion_config(&[typed()], &ordinary).is_err());
        assert!(validate_completion_config(&[goal, typed()], &HookConfigs::default()).is_err());
    }

    #[test]
    fn compaction_only_runs_before_requests_and_is_not_a_completion_mode() {
        let compact = HookConfig::Compaction {
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            context_window: None,
        };
        for stage in 0..4 {
            let mut hooks = HookConfigs::default();
            match stage {
                0 => hooks.agent_create.push(compact.clone()),
                1 => hooks.session_update.push(compact.clone()),
                2 => hooks.before_request.push(compact.clone()),
                _ => hooks.after_request.push(compact.clone()),
            }
            let result = validate_completion_modes(&[typed()], &hooks);
            if stage == 2 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(
                    result,
                    Err(CompletionConfigurationError::InvalidCompactionStage)
                ));
            }
        }
    }

    #[test]
    fn rejects_invalid_schemas_and_nonterminal_stopper_stages() {
        let bundles = [BundleConfig::TypedResult {
            schema: ToolSchema::from(json!({"type":"string"})),
            max_retries: 0,
        }];
        assert!(matches!(
            validate_completion_config(&bundles, &HookConfigs::default()),
            Err(CompletionConfigurationError::InvalidSchema(_))
        ));
        let hooks = HookConfigs {
            before_request: vec![HookConfig::StopWithoutTools],
            ..Default::default()
        };
        assert!(matches!(
            validate_completion_config(&[], &hooks),
            Err(CompletionConfigurationError::InvalidStopperStage)
        ));
    }
}
