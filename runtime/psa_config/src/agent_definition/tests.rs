use super::*;
use psa::api::BundleConfig;

fn configured_defaults() -> AgentDefaults {
    AgentDefaults {
        bundles: vec![
            BundleConfig::Startup {
                base_prompt: true,
                project_context: false,
                cwd: false,
                skills: true,
            },
            BundleConfig::FileTools { read_only: true },
            BundleConfig::StopWithoutTools,
        ],
    }
}

fn lower_config(text: &str, defaults: &AgentDefaults) -> AgentConfig {
    let config: AgentDefinitionConfig = toml::from_str(text).unwrap();
    config.into_agent_config(
        &ConfigGroup::Global,
        Vec::new(),
        &HashSet::new(),
        &HashSet::new(),
        defaults,
    )
}

#[test]
fn omitted_components_inherit_configured_defaults() {
    let defaults = configured_defaults();
    let config = lower_config("name = 'example'", &defaults);
    assert_eq!(config.bundles, defaults.bundles);
}

#[test]
fn explicit_empty_components_disable_configured_defaults() {
    let defaults = configured_defaults();
    let config = lower_config("name = 'example'\nbundles = []", &defaults);
    assert!(config.bundles.is_empty());
}

#[test]
fn parameterized_bundle_configuration_is_parsed_from_toml() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ Startup = { cwd = false } }, { ExtraSystemPrompt = { prompt = 'First prompt' } }, 'StopWithoutTools']",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Startup {
                base_prompt: true,
                project_context: true,
                cwd: false,
                skills: true,
            },
            BundleConfig::ExtraSystemPrompt {
                prompt: "First prompt".into()
            },
            BundleConfig::StopWithoutTools,
        ]
    );
    for invalid in [
        "extra_system_prompt = 'legacy agent field'",
        "[hooks]\nagent_create = ['DefaultStartup']",
        "bundles = ['ExtraSystemPrompt']",
        "bundles = [{ ExtraSystemPrompt = {} }]",
        "bundles = [{ ExtraSystemPrompt = { prompt = 'text', unknown = true } }]",
        "bundles = ['Startup']",
        "tools = []",
        "tools = ['Bash']",
        "tools = ['CreateAgent']",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!("name = 'example'\n{invalid}"))
                .is_err(),
            "accepted invalid component config: {invalid}"
        );
    }
}

#[test]
fn bundle_flag_payloads_parse_from_toml_and_default_to_interactive_only() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ Goal = { interactive_only = false } }, { AskQuestion = {} }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Goal {
                interactive_only: false
            },
            BundleConfig::AskQuestion {
                interactive_only: true
            }
        ]
    );
    let config = lower_config(
        "name = 'example'\nbundles = [{ Goal = {} }, { AskQuestion = { interactive_only = false } }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Goal {
                interactive_only: true
            },
            BundleConfig::AskQuestion {
                interactive_only: false
            }
        ]
    );
    for bundles in [
        "['Goal']",
        "['AskQuestion']",
        "[{ Goal = { unknown = true } }]",
        "[{ AskQuestion = { interactive_only = 'yes' } }]",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!(
                "name = 'example'\nbundles = {bundles}"
            ))
            .is_err()
        );
    }
}

#[test]
fn file_tools_payloads_parse_read_only_and_unit_bundle_selection() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ FileTools = {} }, 'Bash']",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::FileTools { read_only: false },
            BundleConfig::Bash
        ]
    );
    let config = lower_config(
        "name = 'example'\nbundles = [{ FileTools = { read_only = true } }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![BundleConfig::FileTools { read_only: true }]
    );
    for bundles in [
        "['FileTools']",
        "[{ FileTools = { read_only = 'yes' } }]",
        "[{ FileTools = { unknown = true } }]",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!(
                "name = 'example'\nbundles = {bundles}"
            ))
            .is_err()
        );
    }
}

#[test]
fn file_defaults_and_explicit_values() {
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'").unwrap();
    assert!(config.first_model_default);
    assert_eq!(config.models, vec![ModelSelection::Global]);
    assert!(config.skills.global_defaults && config.skills.group_defaults);
    assert!(config.subagents.global_defaults && config.subagents.group_defaults);
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'\ndefault = false\nmodels = []\n[skills]\nglobal_defaults = false\n[subagents]\ngroup_defaults = false").unwrap();
    assert!(!config.first_model_default);
    assert!(config.models.is_empty());
    assert!(!config.skills.global_defaults && config.skills.group_defaults);
    assert!(config.subagents.global_defaults && !config.subagents.group_defaults);
}

#[test]
fn global_group_defaults_include_the_session_group() {
    let available = HashSet::new();
    let global = expand_inclusions(true, true, &[], &ConfigGroup::Global, &available);
    assert_eq!(
        global.groups,
        vec![ConfigGroup::Global, ConfigGroup::Session]
    );
    let global_only = expand_inclusions(true, false, &[], &ConfigGroup::Global, &available);
    assert_eq!(global_only.groups, vec![ConfigGroup::Global]);

    let project = ConfigGroup::Custom("project".into());
    let own = expand_inclusions(true, true, &[], &project, &available);
    assert_eq!(own.groups, vec![ConfigGroup::Global, project]);
}

#[test]
fn parse_rejects_blank_names_and_empty_model_lists() {
    assert!(AgentDefinitionConfig::parse("name = 'example'").is_ok());
    for text in ["name = ' '", "name = 'example'\nmodels = []"] {
        assert!(matches!(
            AgentDefinitionConfig::parse(text),
            Err(AgentDefinitionError::Empty)
        ));
    }
}
