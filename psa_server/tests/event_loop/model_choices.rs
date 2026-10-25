use super::*;
use wire::{EffortSelection, ModelSelection, NewConfiguredAgent};

#[tokio::test]
async fn configured_creation_requires_an_exact_full_model_selection() {
    let harness = Harness::start_with_probe(vec![], false).await;
    let session = harness
        .client
        .create_session(&wire::CreateSession {
            name: "model choices".into(),
            working_directory: "/work".into(),
            probe_id: support::probe_metadata().id,
            model_config: Some(model()),
        })
        .await
        .unwrap();
    let allowed = ModelSelection::Fixed {
        model: "openrouter/vendor/model".into(),
        effort: EffortSelection::Fixed(Effort::High),
    };
    let mut config = agent_config("explicit-model");
    config.models = vec![allowed.clone()];
    config.default = false;
    let registered = harness
        .client
        .upsert_agent_configs(std::slice::from_ref(&config))
        .await
        .unwrap();
    assert!(
        registered[0].config.default,
        "a sole model is normalized to an implicit default"
    );
    assert!(
        harness
            .client
            .get_agent_config(&config.config_id)
            .await
            .unwrap()
            .config
            .default
    );
    let request = |choice| NewConfiguredAgent {
        name: "exact selection".into(),
        config_id: config.config_id.clone(),
        model: choice,
        subagent_parent_id: None,
        interactive: true,
    };

    for choice in [
        Some(ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Fixed(Effort::Low),
        }),
        Some(ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Parent,
        }),
        Some(ModelSelection::Fixed {
            model: "openrouter/vendor/unsupported".into(),
            effort: EffortSelection::Fixed(Effort::High),
        }),
        Some(ModelSelection::Global),
    ] {
        assert!(matches!(
            harness
                .client
                .create_agent(session.id, &request(choice))
                .await,
            Err(wire::ClientError::Api { status: 400, .. })
        ));
    }
    assert!(
        harness
            .client
            .list_agents(session.id)
            .await
            .unwrap()
            .is_empty()
    );

    let implicit = harness
        .client
        .create_agent(session.id, &request(None))
        .await
        .unwrap();
    let implicit_snapshot = harness
        .client
        .get_resolved_agent_config(implicit.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        implicit_snapshot
            .definition
            .model_config
            .as_ref()
            .unwrap()
            .effort,
        Effort::High
    );

    let parent = harness
        .client
        .create_agent(session.id, &request(Some(allowed.clone())))
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(parent.definition_id)
        .await
        .unwrap()
        .unwrap();
    let parent_model = ModelConfig {
        model: Model::new(ProviderType::Openrouter, "vendor/model").unwrap(),
        effort: Effort::High,
    };
    assert_eq!(snapshot.definition.model_config, Some(parent_model.clone()));
    assert_eq!(snapshot.config.models, vec![allowed.clone()]);

    // Policy variants resolve at creation, not while checking allow-list membership.
    let fixed_parent = ModelSelection::Fixed {
        model: "openrouter/vendor/child".into(),
        effort: EffortSelection::Parent,
    };
    let fixed_global = ModelSelection::Fixed {
        model: "openrouter/vendor/child-global".into(),
        effort: EffortSelection::Global,
    };
    let mut dynamic = agent_config("dynamic-model");
    dynamic.default = false;
    dynamic.models = vec![
        ModelSelection::Global,
        ModelSelection::Parent,
        fixed_parent.clone(),
        fixed_global.clone(),
    ];
    let registered = harness
        .client
        .upsert_agent_configs(std::slice::from_ref(&dynamic))
        .await
        .unwrap();
    assert!(!registered[0].config.default);
    let dynamic_request = |choice| NewConfiguredAgent {
        name: "dynamic selection".into(),
        config_id: dynamic.config_id.clone(),
        model: choice,
        subagent_parent_id: Some(parent.id),
        interactive: true,
    };
    assert!(matches!(
        harness
            .client
            .create_agent(session.id, &dynamic_request(None))
            .await,
        Err(wire::ClientError::Api { status: 400, .. })
    ));
    for (index, (selection, expected)) in [
        (ModelSelection::Global, model()),
        (ModelSelection::Parent, parent_model),
        (
            fixed_parent,
            ModelConfig {
                model: Model::new(ProviderType::Openrouter, "vendor/child").unwrap(),
                effort: Effort::High,
            },
        ),
        (
            fixed_global,
            ModelConfig {
                model: Model::new(ProviderType::Openrouter, "vendor/child-global").unwrap(),
                effort: Effort::Medium,
            },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let child = harness
            .client
            .create_agent(
                session.id,
                &NewConfiguredAgent {
                    name: format!("dynamic selection {index}"),
                    config_id: dynamic.config_id.clone(),
                    model: Some(selection),
                    subagent_parent_id: Some(parent.id),
                    interactive: true,
                },
            )
            .await
            .unwrap();
        let snapshot = harness
            .client
            .get_resolved_agent_config(child.definition_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.definition.model_config, Some(expected));
    }

    // Multiple choices require explicit default permission before the first can be implicit.
    let mut with_default = dynamic.clone();
    with_default.default = true;
    harness.upsert_config(with_default).await;
    let defaulted = harness
        .client
        .create_agent(session.id, &dynamic_request(None))
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(defaulted.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.definition.model_config, Some(model()));
    harness.close().await;
}
