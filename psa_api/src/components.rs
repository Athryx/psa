//! Typed construction configuration for hooks and tool-owning feature bundles.
//!
//! Configuration values select and parameterize an installation. The corresponding
//! identity enums describe installed components and do not carry construction data.
//! Unit configurations serialize as variant names; parameterized configurations use
//! externally tagged objects, such as `{"ExtraSystemPrompt":{"prompt":"Be concise."}}`.

use crate::{BundleId, HookId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Construction configuration for a lifecycle hook.
///
/// Hooks run in the order configured for their stage. Each installation carries
/// its own construction data; prompt text belongs to the hook, not the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum HookConfig {
    /// Installs a hook that performs no work.
    Noop,
    /// Stops after a response without tool calls, unless installed goal policy applies.
    StopWithoutTools,
    /// Initializes a new root agent's model and system context.
    DefaultStartup,
    /// Appends a configured system message when this hook runs.
    ExtraSystemPrompt {
        /// System prompt text owned by this hook installation.
        prompt: String,
    },
    /// Summarizes older context before the next inference using a hidden agent.
    Compaction {
        /// Space reserved for the next response; defaults to 16384 tokens.
        #[serde(default = "default_compaction_reserve_tokens")]
        reserve_tokens: u64,
        /// Recent conversation budget to retain verbatim; defaults to 20000 tokens.
        #[serde(default = "default_compaction_keep_recent_tokens")]
        keep_recent_tokens: u64,
        /// Override the model catalog's context window, including for unknown models.
        #[serde(default)]
        context_window: Option<u64>,
    },
    /// Loads project instructions from the working directory hierarchy.
    ProjectContext,
    /// Publishes the installed tools' model-facing definitions.
    ToolDefinitions,
    /// Adds the current session working directory to context.
    Cwd,
    /// Loads the definition's configured skill catalogue.
    Skills,
    /// Follows session model updates unless the definition pins its own model.
    SessionModelUpdate,
}

impl HookConfig {
    /// Return the hook's identity independently of its construction data.
    pub fn id(&self) -> HookId {
        match self {
            Self::Noop => HookId::Noop,
            Self::StopWithoutTools => HookId::StopWithoutTools,
            Self::DefaultStartup => HookId::DefaultStartup,
            Self::ExtraSystemPrompt { .. } => HookId::ExtraSystemPrompt,
            Self::Compaction { .. } => HookId::Compaction,
            Self::ProjectContext => HookId::ProjectContext,
            Self::ToolDefinitions => HookId::ToolDefinitions,
            Self::Cwd => HookId::Cwd,
            Self::Skills => HookId::Skills,
            Self::SessionModelUpdate => HookId::SessionModelUpdate,
        }
    }
}

/// Construction configuration for a feature bundle that owns related components.
///
/// Bundles install all tools in list order before explicit hooks. Bundle ownership
/// keeps shared state and cleanup together instead of independently constructing
/// each component from its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum BundleConfig {
    /// Installs a tool that performs no work.
    Noop,
    /// Installs file tools, optionally restricted to reading.
    FileTools {
        /// When true, install only Read; when false, also install Edit and Write.
        /// Defaults to false when omitted from the payload.
        #[serde(default)]
        read_only: bool,
    },
    /// Installs Bash, InteractBash, and CloseBash with shared process sessions.
    Bash,
    /// Installs the CodeMode execution and reset tools.
    CodeMode,
    /// Installs goal tools and their goal-aware stopping policy.
    Goal {
        /// When true, install only StopWithoutTools for noninteractive agents;
        /// when false, install goal tools and policy for every agent. Defaults to true.
        #[serde(default = "default_interactive_only")]
        interactive_only: bool,
    },
    /// Installs the tool for requesting structured client input.
    AskQuestion {
        /// When true, skip this bundle for noninteractive agents; when false,
        /// install its question tool for every agent. Defaults to true.
        #[serde(default = "default_interactive_only")]
        interactive_only: bool,
    },
    /// Installs session-wide agent tools backed by ancestry and steering.
    Subagents,
    /// Installs schema-validated result recording and its completion policy.
    TypedResult {
        /// Object-root JSON Schema required for recorded result values.
        /// Schema and completion-mode compatibility are validated by the server.
        schema: llm_api::api::ToolSchema,
        /// Maximum result-correction retries; defaults to three when omitted.
        #[serde(default = "default_result_retries")]
        max_retries: u32,
    },
}

impl BundleConfig {
    /// Return the bundle's identity independently of its construction data.
    pub fn id(&self) -> BundleId {
        match self {
            Self::Noop => BundleId::Noop,
            Self::FileTools { .. } => BundleId::FileTools,
            Self::Bash => BundleId::Bash,
            Self::CodeMode => BundleId::CodeMode,
            Self::Goal { .. } => BundleId::Goal,
            Self::AskQuestion { .. } => BundleId::AskQuestion,
            Self::Subagents => BundleId::Subagents,
            Self::TypedResult { .. } => BundleId::TypedResult,
        }
    }

    /// Generate a typed-result configuration with the default correction retry budget.
    /// Uses the schema for values accepted by Serde. The server validates the schema
    /// and requires an object root; this constructor only generates configuration.
    pub fn typed_result<T: JsonSchema>() -> Self {
        Self::typed_result_with_retries::<T>(crate::DEFAULT_RESULT_RETRIES)
    }

    /// Generate a typed-result configuration with an explicit correction retry budget.
    /// A zero budget disables correction retries. This does not validate the schema
    /// or compatibility with other completion policies.
    pub fn typed_result_with_retries<T: JsonSchema>(max_retries: u32) -> Self {
        Self::TypedResult {
            schema: llm_api::api::ToolSchema::for_type::<T>(),
            max_retries,
        }
    }
}

fn default_compaction_reserve_tokens() -> u64 {
    16384
}

fn default_compaction_keep_recent_tokens() -> u64 {
    20000
}

fn default_result_retries() -> u32 {
    crate::DEFAULT_RESULT_RETRIES
}

fn default_interactive_only() -> bool {
    true
}

/// Ordered explicit hook configurations for each lifecycle stage.
///
/// Missing stages deserialize as empty lists. Omission-aware configuration input
/// uses [`crate::HookOverrides`] to distinguish inherited stages from empty ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfigs {
    /// Hooks run only when an agent is first created, not when its worker reloads.
    pub agent_create: Vec<HookConfig>,
    /// Hooks run for session updates, including while the agent is idle.
    pub session_update: Vec<HookConfig>,
    /// Hooks run before an inference request.
    pub before_request: Vec<HookConfig>,
    /// Hooks run after an inference request.
    pub after_request: Vec<HookConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolId;
    use serde_json::json;

    #[test]
    fn component_configurations_preserve_identity_and_roundtrip() {
        for (config, id) in [
            (BundleConfig::Noop, BundleId::Noop),
            (
                BundleConfig::FileTools { read_only: false },
                BundleId::FileTools,
            ),
            (BundleConfig::Bash, BundleId::Bash),
            (BundleConfig::CodeMode, BundleId::CodeMode),
            (
                BundleConfig::Goal {
                    interactive_only: true,
                },
                BundleId::Goal,
            ),
            (
                BundleConfig::AskQuestion {
                    interactive_only: true,
                },
                BundleId::AskQuestion,
            ),
            (BundleConfig::Subagents, BundleId::Subagents),
            (
                BundleConfig::TypedResult {
                    schema: serde_json::json!({"type": "object"}).into(),
                    max_retries: crate::DEFAULT_RESULT_RETRIES,
                },
                BundleId::TypedResult,
            ),
        ] {
            assert_eq!(config.id(), id);
            let wire = serde_json::to_value(&config).unwrap();
            assert_eq!(
                serde_json::from_value::<BundleConfig>(wire).unwrap(),
                config
            );
        }
        for (config, id) in [
            (HookConfig::Noop, HookId::Noop),
            (HookConfig::StopWithoutTools, HookId::StopWithoutTools),
            (HookConfig::DefaultStartup, HookId::DefaultStartup),
            (
                HookConfig::ExtraSystemPrompt {
                    prompt: "Follow these instructions. 🦀".into(),
                },
                HookId::ExtraSystemPrompt,
            ),
            (HookConfig::ProjectContext, HookId::ProjectContext),
            (HookConfig::ToolDefinitions, HookId::ToolDefinitions),
            (HookConfig::Cwd, HookId::Cwd),
            (HookConfig::Skills, HookId::Skills),
            (HookConfig::SessionModelUpdate, HookId::SessionModelUpdate),
        ] {
            assert_eq!(config.id(), id);
            let wire = serde_json::to_value(&config).unwrap();
            assert_eq!(serde_json::from_value::<HookConfig>(wire).unwrap(), config);
        }
    }

    #[test]
    fn removed_tools_fields_are_rejected_even_when_empty() {
        for tools in [json!([]), json!(["Read"]), json!(null)] {
            let definition = json!({"name": "agent", "tools": tools});
            assert!(
                serde_json::from_value::<crate::CreateAgentDefinition>(definition.clone()).is_err()
            );
            assert!(serde_json::from_value::<crate::UpdateAgentDefinition>(definition).is_err());
            let config = json!({
                "config_id": {"config_group": "Global", "name": "agent"},
                "models": ["Global"],
                "tools": tools,
            });
            assert!(serde_json::from_value::<crate::AgentConfigInput>(config.clone()).is_err());
            assert!(serde_json::from_value::<crate::AgentConfig>(config).is_err());
            assert!(
                serde_json::from_value::<crate::AgentDefaults>(json!({"tools": tools})).is_err()
            );
        }
    }

    #[test]
    fn compaction_payload_defaults_and_rejects_invalid_settings() {
        let config: HookConfig =
            serde_json::from_value(serde_json::json!({"Compaction": {}})).unwrap();
        assert_eq!(
            config,
            HookConfig::Compaction {
                reserve_tokens: 16384,
                keep_recent_tokens: 20000,
                context_window: None,
            }
        );
        assert_eq!(config.id(), HookId::Compaction);
        let wire = serde_json::to_value(&config).unwrap();
        assert_eq!(serde_json::from_value::<HookConfig>(wire).unwrap(), config);
        for invalid in [
            serde_json::json!("Compaction"),
            serde_json::json!({"Compaction": {"reserve_tokens": -1}}),
            serde_json::json!({"Compaction": {"keep_recent_tokens": 1.5}}),
            serde_json::json!({"Compaction": {"context_window": "large"}}),
            serde_json::json!({"Compaction": {"unknown": true}}),
        ] {
            assert!(serde_json::from_value::<HookConfig>(invalid).is_err());
        }
    }

    #[test]
    fn file_tools_payload_defaults_to_writable_and_roundtrips_both_flags() {
        let config: BundleConfig = serde_json::from_value(json!({"FileTools": {}})).unwrap();
        assert_eq!(config, BundleConfig::FileTools { read_only: false });
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            json!({"FileTools": {"read_only": false}})
        );
        for read_only in [false, true] {
            let config = BundleConfig::FileTools { read_only };
            assert_eq!(config.id(), BundleId::FileTools);
            let wire = json!({"FileTools": {"read_only": read_only}});
            assert_eq!(serde_json::to_value(&config).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<BundleConfig>(wire).unwrap(),
                config
            );
        }
        for invalid in [
            json!("FileTools"),
            json!({"FileTools": null}),
            json!({"FileTools": {"read_only": null}}),
            json!({"FileTools": {"read_only": "false"}}),
            json!({"FileTools": {"read_only": 0}}),
            json!({"FileTools": {"read_only": true, "unknown": true}}),
            json!({"FileTools": {}, "unknown": true}),
        ] {
            assert!(
                serde_json::from_value::<BundleConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn file_tools_schema_has_a_closed_optional_flag_defaulting_to_false() {
        let schema = serde_json::to_value(schemars::schema_for!(BundleConfig)).unwrap();
        let variant = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"].get("FileTools").is_some())
            .unwrap();
        assert_eq!(variant["additionalProperties"], false);
        assert_eq!(variant["required"], json!(["FileTools"]));
        let payload = &variant["properties"]["FileTools"];
        assert_eq!(payload["additionalProperties"], false);
        assert_eq!(payload["properties"]["read_only"]["type"], "boolean");
        assert_eq!(payload["properties"]["read_only"]["default"], false);
        assert!(payload.get("required").is_none_or(|required| {
            !required.as_array().unwrap().contains(&json!("read_only"))
        }));
        assert!(
            schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["const"] == "Noop")
        );
    }

    #[test]
    fn interactive_bundle_payloads_default_to_true_and_roundtrip_both_flags() {
        for (name, id) in [
            ("Goal", BundleId::Goal),
            ("AskQuestion", BundleId::AskQuestion),
        ] {
            let omitted = json!({name: {}});
            let config: BundleConfig = serde_json::from_value(omitted).unwrap();
            assert_eq!(config.id(), id);
            assert_eq!(
                serde_json::to_value(config).unwrap(),
                json!({name: {"interactive_only": true}})
            );

            for interactive_only in [true, false] {
                let wire = json!({name: {"interactive_only": interactive_only}});
                let config: BundleConfig = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(config.id(), id);
                assert_eq!(serde_json::to_value(config).unwrap(), wire);
            }
            for invalid in [
                json!(name),
                json!({name: null}),
                json!({name: {"interactive_only": null}}),
                json!({name: {"interactive_only": "true"}}),
                json!({name: {"interactive_only": 1}}),
                json!({name: {"unknown": true}}),
                json!({name: {"interactive_only": false, "unknown": true}}),
                json!({name: {}, "unknown": true}),
            ] {
                assert!(
                    serde_json::from_value::<BundleConfig>(invalid.clone()).is_err(),
                    "{invalid}"
                );
            }
        }
    }

    #[test]
    fn interactive_bundle_schemas_describe_optional_closed_boolean_flags() {
        let schema = serde_json::to_value(schemars::schema_for!(BundleConfig)).unwrap();
        for name in ["Goal", "AskQuestion"] {
            let variant = schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|variant| variant["properties"].get(name).is_some())
                .unwrap();
            assert_eq!(variant["additionalProperties"], false);
            assert_eq!(variant["required"], json!([name]));
            let payload = &variant["properties"][name];
            assert_eq!(payload["additionalProperties"], false);
            assert_eq!(payload["properties"]["interactive_only"]["type"], "boolean");
            assert_eq!(payload["properties"]["interactive_only"]["default"], true);
            assert!(payload.get("required").is_none_or(|required| {
                !required
                    .as_array()
                    .unwrap()
                    .contains(&json!("interactive_only"))
            }));
        }
    }

    #[test]
    fn code_mode_config_and_ids_roundtrip_as_unit_variants() {
        let wire = json!("CodeMode");
        assert_eq!(serde_json::to_value(BundleConfig::CodeMode).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<BundleConfig>(wire.clone()).unwrap(),
            BundleConfig::CodeMode
        );
        assert_eq!(serde_json::to_value(BundleId::CodeMode).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<BundleId>(wire.clone()).unwrap(),
            BundleId::CodeMode
        );
        assert_eq!(serde_json::to_value(ToolId::CodeMode).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ToolId>(wire).unwrap(),
            ToolId::CodeMode
        );
        assert!(serde_json::from_value::<BundleConfig>(json!({"CodeMode": {}})).is_err());

        let schema = serde_json::to_value(schemars::schema_for!(BundleConfig)).unwrap();
        assert!(
            schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|variant| variant["const"] == "CodeMode")
        );
    }

    #[test]
    fn reset_code_mode_is_a_tool_identity_not_a_separate_bundle() {
        let wire = json!("ResetCodeMode");
        assert_eq!(serde_json::to_value(ToolId::ResetCodeMode).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ToolId>(wire.clone()).unwrap(),
            ToolId::ResetCodeMode
        );
        assert!(serde_json::from_value::<BundleConfig>(wire.clone()).is_err());
        assert!(serde_json::from_value::<BundleId>(wire).is_err());
        assert!(serde_json::from_value::<ToolId>(json!("reset_codemode")).is_err());
        assert!(serde_json::from_value::<ToolId>(json!({"ResetCodeMode": {}})).is_err());
    }

    #[test]
    fn ask_question_retains_its_tool_identity() {
        assert_eq!(
            serde_json::to_value(ToolId::AskQuestion).unwrap(),
            "AskQuestion"
        );
        assert_eq!(
            serde_json::from_value::<ToolId>(json!("AskQuestion")).unwrap(),
            ToolId::AskQuestion
        );
    }

    #[test]
    fn typed_result_configs_generate_schemas_and_preserve_retry_budgets() {
        type Output = std::collections::BTreeMap<String, u32>;
        let expected_schema = llm_api::api::ToolSchema::for_type::<Output>();
        let default = BundleConfig::typed_result::<Output>();
        assert_eq!(
            default,
            BundleConfig::TypedResult {
                schema: expected_schema.clone(),
                max_retries: crate::DEFAULT_RESULT_RETRIES,
            }
        );
        for max_retries in [0, 1, 5] {
            let config = BundleConfig::typed_result_with_retries::<Output>(max_retries);
            assert_eq!(
                config,
                BundleConfig::TypedResult {
                    schema: expected_schema.clone(),
                    max_retries
                }
            );
            assert_eq!(config.id(), BundleId::TypedResult);
            let wire = serde_json::to_value(&config).unwrap();
            assert_eq!(wire["TypedResult"]["max_retries"], max_retries);
            assert_eq!(wire["TypedResult"]["schema"], *expected_schema.as_value());
            assert_eq!(
                serde_json::from_value::<BundleConfig>(wire).unwrap(),
                config
            );
        }
        let parsed: BundleConfig = serde_json::from_value(json!({
            "TypedResult": {"schema": {"type": "object"}},
        }))
        .unwrap();
        assert_eq!(
            parsed,
            BundleConfig::TypedResult {
                schema: json!({"type": "object"}).into(),
                max_retries: 3,
            }
        );
        for invalid in [
            json!("TypedResult"),
            json!({"TypedResult": {}}),
            json!({"TypedResult": {"schema": {}, "max_retries": -1}}),
            json!({"TypedResult": {"schema": {}, "max_retries": null}}),
            json!({"TypedResult": {"schema": {}, "max_retries": "3"}}),
            json!({"TypedResult": {"schema": {}, "unknown": true}}),
        ] {
            assert!(
                serde_json::from_value::<BundleConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            serde_json::to_value(ToolId::RecordResult).unwrap(),
            "RecordResult"
        );
    }

    #[test]
    fn typed_result_config_schema_requires_a_schema_and_defaults_retries() {
        let schema = serde_json::to_value(schemars::schema_for!(BundleConfig)).unwrap();
        let variant = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"].get("TypedResult").is_some())
            .unwrap();
        let payload = &variant["properties"]["TypedResult"];
        assert_eq!(payload["required"], json!(["schema"]));
        assert_eq!(payload["additionalProperties"], false);
        assert_eq!(payload["properties"]["max_retries"]["default"], 3);
    }

    #[test]
    fn prompt_hook_payload_is_required_closed_and_described_by_schema() {
        let wire = json!({"ExtraSystemPrompt": {"prompt": "Be concise."}});
        let hook: HookConfig = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(hook).unwrap(), wire);
        for invalid in [
            json!("ExtraSystemPrompt"),
            json!({"ExtraSystemPrompt": {}}),
            json!({"ExtraSystemPrompt": {"prompt": null}}),
            json!({"ExtraSystemPrompt": {"prompt": "text", "unknown": true}}),
            json!({"ExtraSystemPrompt": {"prompt": "text"}, "unknown": true}),
        ] {
            assert!(
                serde_json::from_value::<HookConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }

        let schema = serde_json::to_value(schemars::schema_for!(HookConfig)).unwrap();
        let prompt_variant = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"].get("ExtraSystemPrompt").is_some())
            .unwrap();
        assert_eq!(prompt_variant["additionalProperties"], false);
        let payload = &prompt_variant["properties"]["ExtraSystemPrompt"];
        assert_eq!(payload["additionalProperties"], false);
        assert_eq!(payload["required"], json!(["prompt"]));
        assert_eq!(payload["properties"]["prompt"]["type"], "string");
    }

    #[test]
    fn hook_stages_default_to_empty_and_reject_unknown_fields() {
        assert_eq!(
            serde_json::from_value::<HookConfigs>(json!({})).unwrap(),
            HookConfigs::default()
        );
        let hooks = HookConfigs {
            agent_create: vec![HookConfig::ExtraSystemPrompt {
                prompt: "Local prompt".into(),
            }],
            ..Default::default()
        };
        let wire = serde_json::to_value(&hooks).unwrap();
        assert_eq!(serde_json::from_value::<HookConfigs>(wire).unwrap(), hooks);
        assert!(serde_json::from_value::<HookConfigs>(json!({"unknown_stage": []})).is_err());
        assert!(
            serde_json::from_value::<crate::HookOverrides>(json!({"unknown_stage": []})).is_err()
        );
        let schema = serde_json::to_value(schemars::schema_for!(HookConfigs)).unwrap();
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn removed_agent_prompt_fields_are_rejected_by_configuration_inputs() {
        let definition = json!({"name": "agent", "extra_system_prompt": "old field"});
        assert!(
            serde_json::from_value::<crate::CreateAgentDefinition>(definition.clone()).is_err()
        );
        assert!(serde_json::from_value::<crate::UpdateAgentDefinition>(definition).is_err());
        let config = json!({
            "config_id": {"config_group": "Global", "name": "agent"},
            "models": ["Global"],
            "extra_system_prompt": "old field",
        });
        assert!(serde_json::from_value::<crate::AgentConfigInput>(config.clone()).is_err());
        assert!(serde_json::from_value::<crate::AgentConfig>(config).is_err());
    }
}
