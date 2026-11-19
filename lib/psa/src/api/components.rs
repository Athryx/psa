//! Typed construction configuration for feature bundles, which own every installed
//! tool and lifecycle hook.
//!
//! Configuration values select and parameterize an installation. The corresponding
//! identity enums describe installed components and do not carry construction data.
//! Unit configurations serialize as variant names; parameterized configurations use
//! externally tagged objects, such as `{"ExtraSystemPrompt":{"prompt":"Be concise."}}`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default number of result-correction retries for the TypedResult bundle.
pub const DEFAULT_RESULT_RETRIES: u32 = 3;

fn enabled() -> bool {
    true
}

/// One entry of a `DecisionModels` bundle's allowed models, resolved each time
/// the agent calls a decision model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum DecisionModelSelection {
    /// This decision model, written as its slug, such as `openrouter/vendor/model`.
    Fixed(llm_api::providers::Model),
    /// The session's decision model, when the session has one.
    Global,
    /// Every pinned decision model in the catalog, in catalog order.
    Pinned,
}

/// Construction configuration for a feature bundle that owns related components.
///
/// Bundles install their tools and hooks in list order, and hooks run in that
/// order. `Startup` must be listed first: its creation hooks start a new
/// context, clearing anything earlier creation hooks wrote. Bundle ownership keeps
/// shared state and cleanup together instead of independently constructing each
/// component from its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub enum BundleConfig {
    /// Initializes a new root agent's context: model, base instructions, and tool
    /// definitions, plus the selected optional context. Also follows session model
    /// updates unless the definition pins its own model.
    Startup {
        /// Add the base assistant instructions to the new context. When false,
        /// startup only begins the context and sets its model. Defaults to true.
        #[serde(default = "enabled")]
        base_prompt: bool,
        /// Load project instructions from the working directory hierarchy.
        /// Defaults to true.
        #[serde(default = "enabled")]
        project_context: bool,
        /// Add the session working directory to context. Defaults to true.
        #[serde(default = "enabled")]
        cwd: bool,
        /// Load the definition's skill catalogue. Defaults to true.
        #[serde(default = "enabled")]
        skills: bool,
    },
    /// Stops after a response without tool calls. This is the ordinary completion
    /// policy, incompatible with Goal and TypedResult.
    StopWithoutTools,
    /// Appends a configured system message when a root agent is created, after
    /// startup. Forks inherit it with the rest of their context.
    ExtraSystemPrompt {
        /// System prompt text owned by this bundle installation.
        prompt: String,
    },
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
    /// Installs tools for listing psa sessions and exporting them as traces.
    Traces,
    /// Installs the CodeMode functions for calling decision models, restricted to
    /// the models selected here.
    DecisionModels {
        /// Allowed decision models in order, resolved at each call; repeated
        /// models keep their first position. Defaults to empty.
        #[serde(default)]
        models: Vec<DecisionModelSelection>,
        /// Use the first allowed model when a call names none; otherwise calls
        /// must name a model. Defaults to true.
        #[serde(default = "enabled")]
        first_model_default: bool,
    },
    /// Summarizes older context before inference once it nears the context window,
    /// and handles the manual `Compact` command, using a hidden summarizer agent.
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
}

impl BundleConfig {
    /// Return the bundle's identity independently of its construction data.
    pub fn id(&self) -> BundleId {
        match self {
            Self::Startup { .. } => BundleId::Startup,
            Self::StopWithoutTools => BundleId::StopWithoutTools,
            Self::ExtraSystemPrompt { .. } => BundleId::ExtraSystemPrompt,
            Self::FileTools { .. } => BundleId::FileTools,
            Self::Bash => BundleId::Bash,
            Self::CodeMode => BundleId::CodeMode,
            Self::Goal { .. } => BundleId::Goal,
            Self::AskQuestion { .. } => BundleId::AskQuestion,
            Self::Subagents => BundleId::Subagents,
            Self::TypedResult { .. } => BundleId::TypedResult,
            Self::Compaction { .. } => BundleId::Compaction,
            Self::Traces => BundleId::Traces,
            Self::DecisionModels { .. } => BundleId::DecisionModels,
        }
    }

    /// Generate a typed-result configuration with the default correction retry budget.
    /// Uses the schema for values accepted by Serde. The server validates the schema
    /// and requires an object root; this constructor only generates configuration.
    pub fn typed_result<T: JsonSchema>() -> Self {
        Self::typed_result_with_retries::<T>(DEFAULT_RESULT_RETRIES)
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
    DEFAULT_RESULT_RETRIES
}

fn default_interactive_only() -> bool {
    true
}

/// Identity of a built-in feature bundle, without its construction data.
/// Use [`BundleConfig`] when selecting a bundle installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BundleId {
    /// Initializes a new agent's context and follows session model updates.
    Startup,
    /// Installs the ordinary stop-after-a-response-without-tools completion policy.
    StopWithoutTools,
    /// Appends a configured system message on creation.
    ExtraSystemPrompt,
    /// Installs Read and, unless read-only, Edit and Write.
    FileTools,
    /// Installs the Bash, InteractBash, and CloseBash tools with shared sessions.
    Bash,
    /// Installs the CodeMode execution and reset tools.
    CodeMode,
    /// Installs goal tools and a goal-aware stopping policy.
    Goal,
    /// Installs the tool for requesting structured client input.
    AskQuestion,
    /// Installs session-wide agent tools backed by core ancestry and steering.
    Subagents,
    /// Installs schema-validated result recording and its completion policy.
    TypedResult,
    /// Installs automatic context compaction and the `Compact` command.
    Compaction,
    /// Installs session listing and trace export tools.
    Traces,
    /// Installs the CodeMode functions for calling decision models.
    DecisionModels,
}

/// Identity of an installed native agent tool, independent of construction data.
/// Tools are installed through their owning [`BundleConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolId {
    /// Reads file contents.
    Read,
    /// Edits a file using exact text replacements.
    Edit,
    /// Writes file contents.
    Write,
    /// Starts a shell command.
    Bash,
    /// Sends input to or polls a running shell command.
    InteractBash,
    /// Stops a running shell command.
    CloseBash,
    /// Executes code through the CodeMode tool.
    CodeMode,
    /// Resets the CodeMode REPL without deleting prior checkpoints or tool results.
    ResetCodeMode,
    /// Sets the agent's goal when goal mode is installed.
    SetGoal,
    /// Updates the status of the agent's goal.
    SetGoalStatus,
    /// Requests structured input from a connected client.
    AskQuestion,
    /// Creates a subagent from a permitted definition.
    CreateAgent,
    /// Sends an attributed message to another agent.
    SendMessage,
    /// Waits for an agent message.
    WaitMessage,
    /// Lists agents visible to the subagent tools.
    ListAgents,
    /// Records a structured result validated against the installed result schema.
    RecordResult,
    /// Exports a session as a harness-neutral trace directory on the probe.
    ExportTrace,
    /// Lists psa sessions.
    ListSessions,
}

/// A configuration cannot safely establish its selected completion policy.
#[derive(Debug, thiserror::Error)]
pub enum CompletionConfigurationError {
    /// More than one distinct completion policy was selected, including defaults.
    #[error("StopWithoutTools, Goal, and TypedResult completion modes are mutually exclusive")]
    IncompatibleModes,
}

/// Validate completion-policy selection after applying server defaults.
///
/// StopWithoutTools, Goal, and TypedResult are mutually incompatible, even when a
/// bundle's installation policy could skip a particular agent. Duplicate bundles
/// are rejected at installation, not here. The server
/// additionally compiles TypedResult schemas when saving configs.
pub fn validate_completion_modes(
    bundles: &[BundleConfig],
) -> Result<(), CompletionConfigurationError> {
    let selects = |mode: BundleId| bundles.iter().any(|bundle| bundle.id() == mode);
    let modes = [
        BundleId::StopWithoutTools,
        BundleId::Goal,
        BundleId::TypedResult,
    ]
    .into_iter()
    .filter(|mode| selects(*mode))
    .count();
    if modes > 1 {
        return Err(CompletionConfigurationError::IncompatibleModes);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ToolId;
    use llm_api::providers::Model;
    use serde_json::json;

    #[test]
    fn component_configurations_preserve_identity_and_roundtrip() {
        for (config, id) in [
            (
                BundleConfig::Startup {
                    base_prompt: true,
                    project_context: false,
                    cwd: true,
                    skills: false,
                },
                BundleId::Startup,
            ),
            (BundleConfig::StopWithoutTools, BundleId::StopWithoutTools),
            (
                BundleConfig::ExtraSystemPrompt {
                    prompt: "Follow these instructions. 🦀".into(),
                },
                BundleId::ExtraSystemPrompt,
            ),
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
            (BundleConfig::Traces, BundleId::Traces),
            (
                BundleConfig::DecisionModels {
                    models: vec![
                        DecisionModelSelection::Global,
                        DecisionModelSelection::Fixed(
                            Model::parse_slug("openrouter/vendor/decider").unwrap(),
                        ),
                        DecisionModelSelection::Pinned,
                    ],
                    first_model_default: false,
                },
                BundleId::DecisionModels,
            ),
            (
                BundleConfig::TypedResult {
                    schema: serde_json::json!({"type": "object"}).into(),
                    max_retries: DEFAULT_RESULT_RETRIES,
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
    }

    #[test]
    fn decision_models_bundle_defaults_to_nothing_allowed() {
        let config: BundleConfig = serde_json::from_value(json!({"DecisionModels": {}})).unwrap();
        assert_eq!(
            config,
            BundleConfig::DecisionModels {
                models: Vec::new(),
                first_model_default: true,
            }
        );

        let config: BundleConfig = serde_json::from_value(json!({"DecisionModels": {
            "models": [{"Fixed": "openrouter/vendor/a"}, "Global", "Pinned"],
        }}))
        .unwrap();
        let BundleConfig::DecisionModels { models, .. } = config else {
            panic!("not a DecisionModels bundle");
        };
        assert_eq!(
            models[0],
            DecisionModelSelection::Fixed(Model::parse_slug("openrouter/vendor/a").unwrap())
        );
        assert_eq!(
            models[1..],
            [
                DecisionModelSelection::Global,
                DecisionModelSelection::Pinned
            ]
        );
    }

    #[test]
    fn compaction_payload_defaults_and_rejects_invalid_settings() {
        let config: BundleConfig =
            serde_json::from_value(serde_json::json!({"Compaction": {}})).unwrap();
        assert_eq!(
            config,
            BundleConfig::Compaction {
                reserve_tokens: 16384,
                keep_recent_tokens: 20000,
                context_window: None,
            }
        );
        assert_eq!(config.id(), BundleId::Compaction);
        let wire = serde_json::to_value(&config).unwrap();
        assert_eq!(
            serde_json::from_value::<BundleConfig>(wire).unwrap(),
            config
        );
        for invalid in [
            serde_json::json!("Compaction"),
            serde_json::json!({"Compaction": {"reserve_tokens": -1}}),
            serde_json::json!({"Compaction": {"keep_recent_tokens": 1.5}}),
            serde_json::json!({"Compaction": {"context_window": "large"}}),
            serde_json::json!({"Compaction": {"unknown": true}}),
        ] {
            assert!(serde_json::from_value::<BundleConfig>(invalid).is_err());
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
                .any(|variant| variant["const"] == "Bash")
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
                max_retries: DEFAULT_RESULT_RETRIES,
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
    fn prompt_bundle_payload_is_required_closed_and_described_by_schema() {
        let wire = json!({"ExtraSystemPrompt": {"prompt": "Be concise."}});
        let bundle: BundleConfig = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(bundle.id(), BundleId::ExtraSystemPrompt);
        assert_eq!(serde_json::to_value(bundle).unwrap(), wire);
        for invalid in [
            json!("ExtraSystemPrompt"),
            json!({"ExtraSystemPrompt": {}}),
            json!({"ExtraSystemPrompt": {"prompt": null}}),
            json!({"ExtraSystemPrompt": {"prompt": "text", "unknown": true}}),
            json!({"ExtraSystemPrompt": {"prompt": "text"}, "unknown": true}),
        ] {
            assert!(
                serde_json::from_value::<BundleConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }

        let schema = serde_json::to_value(schemars::schema_for!(BundleConfig)).unwrap();
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
    fn startup_payload_flags_default_to_true() {
        let config: BundleConfig = serde_json::from_value(json!({"Startup": {}})).unwrap();
        assert_eq!(
            config,
            BundleConfig::Startup {
                base_prompt: true,
                project_context: true,
                cwd: true,
                skills: true,
            }
        );
        let config: BundleConfig =
            serde_json::from_value(json!({"Startup": {"cwd": false}})).unwrap();
        assert_eq!(
            config,
            BundleConfig::Startup {
                base_prompt: true,
                project_context: true,
                cwd: false,
                skills: true,
            }
        );
        for invalid in [
            json!("Startup"),
            json!({"Startup": null}),
            json!({"Startup": {"skills": "true"}}),
            json!({"Startup": {"unknown": true}}),
        ] {
            assert!(
                serde_json::from_value::<BundleConfig>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            serde_json::to_value(BundleConfig::StopWithoutTools).unwrap(),
            json!("StopWithoutTools")
        );
    }

    fn typed() -> BundleConfig {
        BundleConfig::TypedResult {
            schema: json!({"type":"object"}).into(),
            max_retries: 3,
        }
    }

    #[test]
    fn rejects_incompatible_modes_including_skippable_goal_policy() {
        let ordinary = BundleConfig::StopWithoutTools;
        let goal = BundleConfig::Goal {
            interactive_only: true,
        };
        assert!(validate_completion_modes(&[ordinary.clone()]).is_ok());
        assert!(validate_completion_modes(&[goal.clone()]).is_ok());
        assert!(validate_completion_modes(&[typed()]).is_ok());
        assert!(validate_completion_modes(&[goal.clone(), ordinary.clone()]).is_err());
        assert!(validate_completion_modes(&[typed(), ordinary]).is_err());
        assert!(validate_completion_modes(&[goal, typed()]).is_err());
    }

    #[test]
    fn compaction_is_not_a_completion_mode() {
        let compact = BundleConfig::Compaction {
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            context_window: None,
        };
        assert!(validate_completion_modes(&[compact, typed()]).is_ok());
    }
}
