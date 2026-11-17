use super::select_model;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
use psa::api::{
    AgentConfig, AgentEffort, AgentModel, ConfigGroup, ConfigId, EffortSelection, HookConfigs,
    IncludedItems, ModelSelection,
};

fn session() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test/global").unwrap(),
        effort: Effort::Medium,
    }
}

fn fixed_parent() -> AgentModel {
    AgentModel::Fixed {
        model: Model::new(ProviderType::OpenAiCodex, "parent-model").unwrap(),
        effort: AgentEffort::Fixed(Effort::High),
    }
}

fn vendor_model() -> Model {
    Model::new(ProviderType::Openrouter, "vendor/model").unwrap()
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
    let parent = fixed_parent();
    let mut config = config(vec![ModelSelection::Global, ModelSelection::Parent], false);
    assert!(select_model(&config, None, &parent).is_err());
    let unsupported = ModelSelection::Fixed {
        model: "openrouter/other".parse().unwrap(),
        effort: EffortSelection::Global,
    };
    assert!(select_model(&config, Some(&unsupported), &parent).is_err());
    assert_eq!(
        select_model(&config, Some(&ModelSelection::Parent), &parent).unwrap(),
        parent
    );
    assert_eq!(
        select_model(&config, Some(&ModelSelection::Global), &parent).unwrap(),
        AgentModel::Global
    );
    config.default = true;
    assert_eq!(
        select_model(&config, None, &parent).unwrap(),
        AgentModel::Global
    );
    config.models.clear();
    assert!(select_model(&config, None, &AgentModel::Global).is_err());
}

#[test]
fn parent_choices_copy_the_parent_policy_so_global_parents_keep_children_following() {
    let config = config(vec![ModelSelection::Parent], false);
    assert_eq!(
        select_model(&config, None, &AgentModel::Global).unwrap(),
        AgentModel::Global
    );
    assert_eq!(
        select_model(&config, None, &fixed_parent()).unwrap(),
        fixed_parent()
    );
}

#[test]
fn a_single_allowed_model_is_implicitly_selected_even_without_default_permission() {
    for selection in [
        ModelSelection::Global,
        ModelSelection::Parent,
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".parse().unwrap(),
            effort: EffortSelection::Fixed(Effort::Low),
        },
    ] {
        let config = config(vec![selection.clone()], false);
        let parent = AgentModel::Global;
        let selected = select_model(&config, Some(&selection), &parent).unwrap();
        assert_eq!(select_model(&config, None, &parent).unwrap(), selected);
    }
}

#[test]
fn fixed_model_choices_require_exact_slug_and_effort_policy() {
    let allowed = ModelSelection::Fixed {
        model: "openrouter/vendor/model".parse().unwrap(),
        effort: EffortSelection::Parent,
    };
    let config = config(vec![allowed.clone()], false);
    let parent = fixed_parent();
    assert_eq!(
        select_model(&config, Some(&allowed), &parent).unwrap(),
        AgentModel::Fixed {
            model: vendor_model(),
            effort: AgentEffort::Fixed(Effort::High),
        }
    );

    for unsupported in [
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".parse().unwrap(),
            // Even a fixed effort equal to the parent's effort is a different policy.
            effort: EffortSelection::Fixed(Effort::High),
        },
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".parse().unwrap(),
            effort: EffortSelection::Global,
        },
        ModelSelection::Fixed {
            model: "openrouter/vendor/other".parse().unwrap(),
            effort: EffortSelection::Parent,
        },
        ModelSelection::Fixed {
            model: "openai_codex/vendor/model".parse().unwrap(),
            effort: EffortSelection::Parent,
        },
        ModelSelection::Global,
        ModelSelection::Parent,
    ] {
        assert!(select_model(&config, Some(&unsupported), &parent).is_err());
    }
}

#[test]
fn fixed_models_parse_provider_slug_and_keep_effort_policy() {
    for (effort, parent, expected) in [
        (
            EffortSelection::Parent,
            fixed_parent(),
            AgentEffort::Fixed(Effort::High),
        ),
        // Root agents and children of global parents follow the session effort.
        (
            EffortSelection::Parent,
            AgentModel::Global,
            AgentEffort::Global,
        ),
        (EffortSelection::Global, fixed_parent(), AgentEffort::Global),
        (
            EffortSelection::Fixed(Effort::Low),
            AgentModel::Global,
            AgentEffort::Fixed(Effort::Low),
        ),
    ] {
        let selection = ModelSelection::Fixed {
            model: "openrouter/vendor/model".parse().unwrap(),
            effort,
        };
        let config = config(vec![selection.clone()], false);
        assert_eq!(
            select_model(&config, Some(&selection), &parent).unwrap(),
            AgentModel::Fixed {
                model: vendor_model(),
                effort: expected,
            }
        );
    }
}

#[test]
fn global_policies_resolve_against_the_current_session() {
    let session = session();
    assert_eq!(AgentModel::Global.resolve(&session), session);
    let fixed_model = AgentModel::Fixed {
        model: vendor_model(),
        effort: AgentEffort::Global,
    };
    assert_eq!(
        fixed_model.resolve(&session),
        ModelConfig {
            model: vendor_model(),
            effort: Effort::Medium,
        }
    );
    assert_eq!(fixed_parent().resolve(&session).effort, Effort::High);
}
