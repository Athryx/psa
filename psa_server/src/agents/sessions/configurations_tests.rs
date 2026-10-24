use super::select_model;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa_api::{
    AgentConfig, ConfigGroup, ConfigId, EffortSelection, HookIds, IncludedItems, ModelSelection,
};

fn global() -> ModelConfig {
    ModelConfig {
        model: Model::new("test/global").unwrap(),
        provider: ProviderType::Openrouter,
        effort: Effort::Medium,
    }
}

fn config(models: Vec<ModelSelection>, default: bool) -> AgentConfig {
    AgentConfig {
        config_id: ConfigId {
            config_group: ConfigGroup::Custom("tests".into()),
            name: "agent".into(),
        },
        description: String::new(),
        selectable: true,
        bundles: vec![],
        tools: vec![],
        hooks: HookIds::default(),
        extra_system_prompt: String::new(),
        models,
        default,
        subagents: IncludedItems::default(),
        skills: IncludedItems::default(),
    }
}

#[test]
fn model_choices_enforce_allowed_list_and_default_policy() {
    let global = global();
    let config = config(vec![ModelSelection::Global, ModelSelection::Parent], false);
    assert!(select_model(&config, None, &global, None).is_err());
    assert!(select_model(&config, Some("other"), &global, None).is_err());
    assert_eq!(
        select_model(&config, Some("parent"), &global, None).unwrap(),
        global
    );
    let mut config = config;
    config.default = true;
    assert_eq!(select_model(&config, None, &global, None).unwrap(), global);
    config.models.clear();
    assert!(select_model(&config, None, &global, None).is_err());
}

#[test]
fn fixed_models_parse_provider_slug_and_resolve_effort_at_creation() {
    let global = global();
    let parent = ModelConfig {
        effort: Effort::High,
        ..global.clone()
    };
    let mut config = config(
        vec![ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Parent,
        }],
        false,
    );
    let selected = select_model(&config, None, &global, Some(&parent)).unwrap();
    assert_eq!(selected.model.api_name, "vendor/model");
    assert_eq!(selected.effort, Effort::High);
    assert_eq!(
        select_model(&config, None, &global, None).unwrap().effort,
        global.effort
    );
    config.models = vec![ModelSelection::Parent];
    assert_eq!(
        select_model(&config, None, &global, Some(&parent)).unwrap(),
        parent
    );
    config.models = vec![ModelSelection::Fixed {
        model: "unknown/model".into(),
        effort: EffortSelection::Global,
    }];
    assert!(select_model(&config, None, &global, Some(&parent)).is_err());
}
