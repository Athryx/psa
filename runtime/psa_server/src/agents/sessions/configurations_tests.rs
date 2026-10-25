use super::select_model;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa_api::{
    AgentConfig, ConfigGroup, ConfigId, EffortSelection, HookConfigs, IncludedItems, ModelSelection,
};

fn global() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test/global").unwrap(),
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
        hooks: HookConfigs::default(),
        models,
        default,
        subagents: IncludedItems::default(),
        skills: IncludedItems::default(),
    }
}

#[test]
fn model_choices_enforce_allowed_list_and_default_policy() {
    let global = global();
    let parent = ModelConfig {
        model: Model::new(ProviderType::OpenAiCodex, "parent-model").unwrap(),
        effort: Effort::High,
    };
    let mut config = config(vec![ModelSelection::Global, ModelSelection::Parent], false);
    assert!(select_model(&config, None, &global, Some(&parent)).is_err());
    let unsupported = ModelSelection::Fixed {
        model: "openrouter/other".into(),
        effort: EffortSelection::Global,
    };
    assert!(select_model(&config, Some(&unsupported), &global, Some(&parent)).is_err());
    assert_eq!(
        select_model(
            &config,
            Some(&ModelSelection::Parent),
            &global,
            Some(&parent)
        )
        .unwrap(),
        parent
    );
    assert_eq!(
        select_model(
            &config,
            Some(&ModelSelection::Global),
            &global,
            Some(&parent)
        )
        .unwrap(),
        global
    );
    assert_eq!(
        select_model(&config, Some(&ModelSelection::Parent), &global, None).unwrap(),
        global
    );
    config.default = true;
    assert_eq!(
        select_model(&config, None, &global, Some(&parent)).unwrap(),
        global
    );
    config.models.clear();
    assert!(select_model(&config, None, &global, None).is_err());
}

#[test]
fn a_single_allowed_model_is_implicitly_selected_even_without_default_permission() {
    let global = global();
    for selection in [
        ModelSelection::Global,
        ModelSelection::Parent,
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Fixed(Effort::Low),
        },
    ] {
        let config = config(vec![selection.clone()], false);
        let selected = select_model(&config, Some(&selection), &global, None).unwrap();
        assert_eq!(
            select_model(&config, None, &global, None).unwrap(),
            selected
        );
    }
}

#[test]
fn fixed_model_choices_require_exact_slug_and_effort_policy() {
    let global = global();
    let parent = ModelConfig {
        effort: Effort::High,
        ..global.clone()
    };
    let allowed = ModelSelection::Fixed {
        model: "openrouter/vendor/model".into(),
        effort: EffortSelection::Parent,
    };
    let config = config(vec![allowed.clone()], false);
    let selected = select_model(&config, Some(&allowed), &global, Some(&parent)).unwrap();
    assert_eq!(
        selected.model,
        Model::new(ProviderType::Openrouter, "vendor/model").unwrap()
    );
    assert_eq!(selected.effort, Effort::High);

    for unsupported in [
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            // Even a fixed effort equal to the parent's resolved effort is a different policy.
            effort: EffortSelection::Fixed(Effort::High),
        },
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Global,
        },
        ModelSelection::Fixed {
            model: "openrouter/vendor/other".into(),
            effort: EffortSelection::Parent,
        },
        ModelSelection::Fixed {
            model: "openai_codex/vendor/model".into(),
            effort: EffortSelection::Parent,
        },
        ModelSelection::Global,
        ModelSelection::Parent,
    ] {
        assert!(select_model(&config, Some(&unsupported), &global, Some(&parent)).is_err());
    }
}

#[test]
fn fixed_models_parse_provider_slug_and_resolve_effort_at_creation() {
    let global = global();
    let parent = ModelConfig {
        effort: Effort::High,
        ..global.clone()
    };
    for (effort, expected) in [
        (EffortSelection::Parent, Effort::High),
        (EffortSelection::Global, global.effort),
        (EffortSelection::Fixed(Effort::Low), Effort::Low),
    ] {
        let selection = ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort,
        };
        let config = config(vec![selection.clone()], false);
        let selected = select_model(&config, Some(&selection), &global, Some(&parent)).unwrap();
        assert_eq!(selected.model.api_name, "vendor/model");
        assert_eq!(selected.effort, expected);
        let fallback = select_model(&config, Some(&selection), &global, None).unwrap();
        assert_eq!(
            fallback.effort,
            if matches!(
                selection,
                ModelSelection::Fixed {
                    effort: EffortSelection::Fixed(_),
                    ..
                }
            ) {
                Effort::Low
            } else {
                global.effort
            }
        );
    }
    let invalid = ModelSelection::Fixed {
        model: "unknown/model".into(),
        effort: EffortSelection::Global,
    };
    let config = config(vec![invalid.clone()], false);
    assert!(select_model(&config, Some(&invalid), &global, Some(&parent)).is_err());
}
