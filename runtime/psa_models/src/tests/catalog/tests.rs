use super::*;
use axum::{
    Router,
    body::Body,
    extract::{Path, State as HttpState},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use llm_api::providers::Effort;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};
use tokio::{net::TcpListener, sync::Notify};

#[derive(Default)]
struct Fake {
    requests: AtomicUsize,
    validators: AtomicUsize,
    mode: AtomicU8,
    gate: Notify,
}

async fn catalog(
    HttpState(fake): HttpState<Arc<Fake>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let mode = fake.mode.load(Ordering::SeqCst);
    if mode == 4 || (mode == 6 && provider == "openai-codex") {
        fake.gate.notified().await;
    }
    if mode == 1 || (matches!(mode, 5 | 6) && provider == "openrouter") {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .unwrap();
    }
    if mode == 2 {
        return Response::builder()
            .body(Body::from("invalid JSON"))
            .unwrap();
    }
    if mode == 3 {
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from("[]"))
            .unwrap();
    }
    if headers.get("if-none-match").is_some() && mode == 0 {
        fake.validators.fetch_add(1, Ordering::SeqCst);
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap();
    }
    let context_window = if matches!(mode, 5 | 6) {
        200000
    } else {
        100000
    };
    let body = json!([{"id":"vendor/model","name":"Test","type":"chat","reasoning":true,
        "thinkingLevelMap":{"minimal":"low","max":"max","off":null},"contextWindow":context_window,"maxTokens":4096,
        "cost":{"input":1,"output":4}}]);
    Response::builder()
        .header("etag", "\"test-v1\"")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

struct Fixture {
    directory: tempfile::TempDir,
    database: Database,
    fake: Arc<Fake>,
    source: CatalogSource,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("models.sqlite3"))
            .await
            .unwrap();
        let fake = Arc::new(Fake::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source =
            CatalogSource::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let app = Router::new()
            .route("/api/models/providers/{provider}", get(catalog))
            .with_state(fake.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            database,
            fake,
            source,
            server,
        }
    }

    async fn synced_runtime(&self) -> ModelCatalogRuntime {
        let registry = ProviderRegistry::new(vec![
            llm_api::api::Provider::new(ProviderType::Openrouter, "test-key").unwrap(),
        ]);
        let runtime =
            ModelCatalogRuntime::start(self.database.clone(), &registry, self.source.clone())
                .await
                .unwrap();
        runtime.handle.resync(None).await.unwrap();
        runtime
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn config() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "vendor/model").unwrap(),
        effort: Effort::Minimal,
    }
}

#[tokio::test]
async fn discovery_filters_lookup_reasoning_and_etags() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let providers = runtime.handle.providers();
    assert_eq!(providers.len(), 2);
    assert!(
        providers
            .iter()
            .find(|provider| provider.id == ProviderType::Openrouter)
            .unwrap()
            .authed
    );
    assert!(
        !providers
            .iter()
            .find(|provider| provider.id == ProviderType::OpenAiCodex)
            .unwrap()
            .authed
    );
    assert_eq!(
        runtime
            .handle
            .list_models(ListModels::default())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        runtime
            .handle
            .list_models(ListModels {
                authed: Some(false),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        runtime
            .handle
            .model_info(&config().model)
            .await
            .unwrap()
            .unwrap()
            .context_window,
        Some(100000)
    );
    assert_eq!(
        runtime.handle.resolve_reasoning(&config()).await.unwrap(),
        Some(ReasoningOverride::Effort(Effort::Low))
    );
    let mut off = config();
    off.effort = Effort::Off;
    assert!(
        runtime
            .handle
            .resolve_reasoning(&off)
            .await
            .unwrap_err()
            .to_string()
            .contains("Supported:")
    );
    off.model = Model::new(ProviderType::Openrouter, "custom/unknown").unwrap();
    assert_eq!(runtime.handle.resolve_reasoning(&off).await.unwrap(), None);
    assert!(
        runtime
            .handle
            .model_info(&off.model)
            .await
            .unwrap()
            .is_none()
    );
    runtime.handle.resync(None).await.unwrap();
    assert!(fixture.fake.validators.load(Ordering::SeqCst) >= 2);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn credential_updates_are_visible_across_catalog_views() {
    let fixture = Fixture::new().await;
    let registry =
        ProviderRegistry::default().with_auth_path(fixture.directory.path().join("auth.toml"));
    let mut runtime =
        ModelCatalogRuntime::start(fixture.database.clone(), &registry, fixture.source.clone())
            .await
            .unwrap();
    runtime.handle.resync(None).await.unwrap();
    let model = config().model;
    for (credential, expected) in [(Some("secret".to_owned()), true), (None, false)] {
        runtime
            .handle
            .set_provider_credential(psa::api::SetProviderCredential {
                provider: ProviderType::Openrouter,
                credential,
            })
            .await
            .unwrap();
        assert_eq!(
            runtime
                .handle
                .providers()
                .into_iter()
                .find(|p| p.id == ProviderType::Openrouter)
                .unwrap()
                .authed,
            expected
        );
        assert_eq!(
            runtime
                .handle
                .model_info(&model)
                .await
                .unwrap()
                .unwrap()
                .authed,
            expected
        );
        assert_eq!(
            runtime
                .handle
                .set_pinned(PinModel {
                    model: model.clone(),
                    pinned: true
                })
                .await
                .unwrap()
                .unwrap()
                .authed,
            expected
        );
        let models = runtime
            .handle
            .list_models(ListModels {
                provider: Some(ProviderType::Openrouter),
                authed: Some(expected),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!models.is_empty());
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn pins_survive_replacements_disappearance_and_restart() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let pin = PinModel {
        model: config().model,
        pinned: true,
    };
    assert!(
        runtime
            .handle
            .set_pinned(pin.clone())
            .await
            .unwrap()
            .unwrap()
            .pinned
    );
    assert_eq!(
        runtime
            .handle
            .list_models(ListModels {
                authed: Some(true),
                pinned: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        runtime
            .handle
            .list_models(ListModels {
                provider: Some(ProviderType::OpenAiCodex),
                pinned: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty()
    );
    fixture.fake.mode.store(3, Ordering::SeqCst);
    runtime.handle.resync(None).await.unwrap();
    assert!(
        runtime
            .handle
            .list_models(ListModels::default())
            .await
            .unwrap()
            .is_empty()
    );
    fixture.fake.mode.store(4, Ordering::SeqCst);
    fixture.fake.gate.notify_one();
    let restored = runtime.handle.resync(Some(ProviderType::Openrouter));
    // Mode 4 always sends a body, proving pins are independent of catalog JSON.
    restored.await.unwrap();
    assert!(
        runtime
            .handle
            .model_info(&config().model)
            .await
            .unwrap()
            .unwrap()
            .pinned
    );
    runtime.shutdown().await.unwrap();
    fixture.database.close().await;

    let database = Database::open(fixture.directory.path().join("models.sqlite3"))
        .await
        .unwrap();
    let mut restarted = ModelCatalogRuntime::start(
        database.clone(),
        &ProviderRegistry::default(),
        fixture.source.clone(),
    )
    .await
    .unwrap();
    assert!(
        restarted
            .handle
            .model_info(&config().model)
            .await
            .unwrap()
            .unwrap()
            .pinned
    );
    assert!(
        !restarted
            .handle
            .model_info(&config().model)
            .await
            .unwrap()
            .unwrap()
            .authed
    );
    let mut unknown = pin.clone();
    unknown.model = Model::new(ProviderType::Openrouter, "missing").unwrap();
    assert!(matches!(
        restarted.handle.set_pinned(unknown.clone()).await,
        Err(CatalogError::NotFound)
    ));
    unknown.pinned = false;
    assert!(
        restarted
            .handle
            .set_pinned(unknown)
            .await
            .unwrap()
            .is_none()
    );
    let mut unpin = pin;
    unpin.pinned = false;
    assert!(
        !restarted
            .handle
            .set_pinned(unpin)
            .await
            .unwrap()
            .unwrap()
            .pinned
    );
    restarted.shutdown().await.unwrap();
    database.close().await;
}

#[tokio::test]
async fn identical_api_names_have_provider_specific_lookups_and_pins() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let openrouter = Model::new(ProviderType::Openrouter, "vendor/model").unwrap();
    let codex = Model::new(ProviderType::OpenAiCodex, "vendor/model").unwrap();

    for model in [&openrouter, &codex] {
        let info = runtime.handle.model_info(model).await.unwrap().unwrap();
        assert_eq!(&info.model, model);
        assert!(!info.pinned);
    }

    runtime
        .handle
        .set_pinned(PinModel {
            model: openrouter.clone(),
            pinned: true,
        })
        .await
        .unwrap();
    let openrouter_info = runtime
        .handle
        .model_info(&openrouter)
        .await
        .unwrap()
        .unwrap();
    let codex_info = runtime.handle.model_info(&codex).await.unwrap().unwrap();
    assert!(openrouter_info.pinned);
    assert!(openrouter_info.authed);
    assert!(!codex_info.pinned);
    assert!(!codex_info.authed);

    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_and_invalid_refreshes_keep_last_good_catalog() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    for mode in [1, 2, 5] {
        fixture.fake.mode.store(mode, Ordering::SeqCst);
        let error = runtime.handle.resync(None).await.unwrap_err();
        let CatalogError::SyncFailed(message) = error else {
            panic!("expected aggregated sync failure, got {error:?}");
        };
        assert!(message.contains(ProviderType::Openrouter.name()));
        assert_eq!(
            message.contains(ProviderType::OpenAiCodex.name()),
            mode != 5
        );
        assert_eq!(
            runtime
                .handle
                .list_models(ListModels::default())
                .await
                .unwrap()
                .len(),
            2
        );
        for provider in ProviderType::ALL {
            let model = Model::new(provider, "vendor/model").unwrap();
            let info = runtime.handle.model_info(&model).await.unwrap().unwrap();
            let expected_context = if mode == 5 && provider == ProviderType::OpenAiCodex {
                200000
            } else {
                100000
            };
            assert_eq!(info.context_window, Some(expected_context));
        }
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_resync_waits_for_all_requested_providers() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let before = fixture.fake.requests.load(Ordering::SeqCst);
    fixture.fake.mode.store(6, Ordering::SeqCst);
    let handle = runtime.handle.clone();
    let all = tokio::spawn(async move { handle.resync(None).await });

    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.fake.requests.load(Ordering::SeqCst) < before + 2 {
            tokio::task::yield_now().await;
        }
        // This waiter observes a completed failure while the Codex request stays blocked.
        let failed = runtime.handle.resync(Some(ProviderType::Openrouter)).await;
        assert!(matches!(failed, Err(CatalogError::SyncFailed(_))));
    })
    .await
    .unwrap();
    assert!(
        !all.is_finished(),
        "the all-provider waiter returned before Codex finished"
    );

    fixture.fake.gate.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(3), all)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(CatalogError::SyncFailed(_))));
    let codex = Model::new(ProviderType::OpenAiCodex, "vendor/model").unwrap();
    let info = runtime.handle.model_info(&codex).await.unwrap().unwrap();
    assert_eq!(info.context_window, Some(200000));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_manual_refreshes_share_work_and_shutdown_cancels_fetches() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let before = fixture.fake.requests.load(Ordering::SeqCst);
    fixture.fake.mode.store(4, Ordering::SeqCst);
    let first = runtime.handle.resync(Some(ProviderType::Openrouter));
    let second = runtime.handle.resync(Some(ProviderType::Openrouter));
    let release = async {
        while fixture.fake.requests.load(Ordering::SeqCst) == before {
            tokio::task::yield_now().await;
        }
        // Both callers are registered before the blocked HTTP response completes.
        tokio::task::yield_now().await;
        fixture.fake.gate.notify_one();
    };
    let (first, second, ()) = tokio::join!(first, second, release);
    first.unwrap();
    second.unwrap();
    assert_eq!(fixture.fake.requests.load(Ordering::SeqCst), before + 1);
    let handle = runtime.handle.clone();
    let blocked = tokio::spawn(async move { handle.resync(None).await });
    while fixture.fake.requests.load(Ordering::SeqCst) < before + 3 {
        tokio::task::yield_now().await;
    }
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        blocked.await.unwrap(),
        Err(CatalogError::Unavailable)
    ));
}

#[tokio::test]
async fn scheduled_refresh_happens_at_four_hours_and_fresh_start_uses_database() {
    let fixture = Fixture::new().await;
    let mut runtime = fixture.synced_runtime().await;
    let before = fixture.fake.requests.load(Ordering::SeqCst);
    tokio::time::pause();
    tokio::time::advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(fixture.fake.requests.load(Ordering::SeqCst), before);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.fake.requests.load(Ordering::SeqCst) < before + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Join the scheduled refreshes before restarting.
    runtime.handle.resync(None).await.unwrap();
    runtime.shutdown().await.unwrap();
    let before = fixture.fake.requests.load(Ordering::SeqCst);
    let mut restarted = ModelCatalogRuntime::start(
        fixture.database.clone(),
        &ProviderRegistry::default(),
        fixture.source.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        restarted
            .handle
            .list_models(ListModels::default())
            .await
            .unwrap()
            .len(),
        2
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(fixture.fake.requests.load(Ordering::SeqCst), before);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_start_can_list_providers_and_empty_models_while_network_is_blocked() {
    let fixture = Fixture::new().await;
    fixture.fake.mode.store(4, Ordering::SeqCst);
    let mut runtime = ModelCatalogRuntime::start(
        fixture.database.clone(),
        &ProviderRegistry::default(),
        fixture.source.clone(),
    )
    .await
    .unwrap();
    assert!(
        runtime
            .handle
            .providers()
            .iter()
            .all(|provider| !provider.authed)
    );
    assert!(
        runtime
            .handle
            .list_models(ListModels::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime
            .handle
            .model_info(&config().model)
            .await
            .unwrap()
            .is_none()
    );
    runtime.shutdown().await.unwrap();
}
