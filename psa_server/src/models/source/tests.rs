use super::*;
use serde_json::json;

#[test]
fn normalizes_prices_limits_names_and_reasoning_without_importing_transport() {
    let models = normalize_catalog(
        ProviderType::Openrouter,
        json!([
            {"id":"vendor/model","type":"chat","name":"Friendly","reasoning":true,
             "contextWindow":128000,"maxTokens":8192,"input":["text","image"],
             "thinkingLevelMap":{"off":null,"minimal":"low","xhigh":"high","max":"max"},
             "cost":{"input":2.5,"output":10,"cacheRead":0.25,"cacheWrite":0,
                 "tiers":[{"inputTokensAbove":100000,"input":5,"output":20}]},
             "baseUrl":"https://untrusted.invalid","headers":{"Authorization":"do not import"}},
            {"id":"plain","reasoning":false},
            {"id":"image-only","type":"image"}
        ]),
    )
    .unwrap();
    assert_eq!(models.len(), 2);
    let model = &models[0];
    assert_eq!(model.model.provider, ProviderType::Openrouter);
    assert_eq!(model.context_window, Some(128000));
    assert_eq!(model.max_output_tokens, Some(8192));
    assert_eq!(model.cost.rates.input, Some(2.5));
    assert_eq!(model.cost.rates.cache_write, Some(0.0));
    assert_eq!(model.cost.tiers[0].input_tokens_above, 100000);
    assert_eq!(model.cost.tiers[0].rates.cache_read, None);
    assert!(
        !model
            .reasoning_options
            .iter()
            .any(|option| option.effort == Effort::Off)
    );
    assert_eq!(
        model
            .reasoning_options
            .iter()
            .find(|option| option.effort == Effort::Minimal)
            .unwrap()
            .wire_effort,
        Some(Effort::Low)
    );
    assert_eq!(
        model
            .reasoning_options
            .iter()
            .find(|option| option.effort == Effort::Max)
            .unwrap()
            .wire_effort,
        Some(Effort::Max)
    );
    assert!(!serde_json::to_string(model).unwrap().contains("untrusted"));
    assert_eq!(models[1].name, "plain");
    assert_eq!(models[1].cost.rates.input, None);
    assert_eq!(models[1].context_window, None);
    assert_eq!(models[1].reasoning_options[0].effort, Effort::Off);
    assert_eq!(models[1].reasoning_options[0].wire_effort, None);
}

#[test]
fn pi_defaults_require_explicit_support_for_extended_levels() {
    let options = normalize_reasoning(true, &HashMap::new()).unwrap();
    assert_eq!(
        options
            .iter()
            .map(|option| option.effort)
            .collect::<Vec<_>>(),
        vec![
            Effort::Off,
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High
        ]
    );
    let mapped =
        normalize_reasoning(true, &HashMap::from([("off".into(), Some("none".into()))])).unwrap();
    assert_eq!(mapped[0].wire_effort, Some(Effort::Off));
}

#[test]
fn supports_catalog_shapes_and_rejects_invalid_replacements() {
    for catalog in [
        json!({"models":[{"id":"test","reasoning":false}]}),
        json!({"test":{"id":"test","reasoning":false}}),
    ] {
        assert_eq!(
            normalize_catalog(ProviderType::OpenAiCodex, catalog)
                .unwrap()
                .len(),
            1
        );
    }
    for catalog in [
        json!(null),
        json!({"models":{}}),
        json!([{"id":" bad ","reasoning":true}]),
        json!([{"id":"test","reasoning":true,"cost":{"input":-1}}]),
        json!([{"id":"test","reasoning":true,"thinkingLevelMap":{"high":"unknown"}}]),
        json!([{"id":"test","reasoning":false},{"id":"test","reasoning":false}]),
    ] {
        assert!(normalize_catalog(ProviderType::Openrouter, catalog).is_err());
    }
}

#[tokio::test]
#[ignore = "requires network access to the public Pi catalog"]
async fn live_pi_catalogs_normalize_for_supported_providers() {
    let source = CatalogSource::pi();
    for provider in ProviderType::ALL {
        let CatalogUpdate::Models { models, .. } = source
            .fetch(provider, &ProviderSyncState::default())
            .await
            .unwrap()
        else {
            panic!("expected catalog body");
        };
        assert!(!models.is_empty());
        assert!(models.iter().all(|model| model.model.provider == provider));
        eprintln!("{provider:?}: {} chat models", models.len());
    }
}
