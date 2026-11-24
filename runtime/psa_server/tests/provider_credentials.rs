use llm_api::providers::ProviderType;
use psa::api::{Client, SetProviderCredential};
use psa_models::ProviderRegistry;
use psa_store::Database;
use tokio::{net::TcpListener, sync::oneshot};

#[tokio::test]
async fn authenticated_credential_updates_are_live_and_never_return_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.toml");
    tokio::fs::write(&path, "[psa]\nclient_key = 'client'\nprobe_key = 'probe'\n")
        .await
        .unwrap();
    let providers = ProviderRegistry::load(&path)
        .await
        .unwrap()
        .with_auth_path(path.clone());
    let database = Database::open(dir.path().join("db.sqlite")).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let client = Client::new(&base, "client").unwrap();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(psa_server::server::serve_with_providers(
        listener,
        database,
        providers.clone(),
        "client".into(),
        async {
            let _ = stopped.await;
        },
    ));
    let http = reqwest::Client::new();
    let body = serde_json::json!({"provider":"Openrouter", "credential":"SECRET_SENTINEL"});
    let response = http
        .put(format!("{base}/providers/credential"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(providers.provider(ProviderType::Openrouter).is_none());
    let response = http
        .put(format!("{base}/providers/credential"))
        .bearer_auth("client")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(!text.contains("SECRET_SENTINEL"));
    assert!(providers.provider(ProviderType::Openrouter).is_some());
    assert!(
        client
            .list_providers()
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == ProviderType::Openrouter && p.authed)
    );
    let malformed = http
        .put(format!("{base}/providers/credential"))
        .bearer_auth("client")
        .body("{\"provider\":\"SECRET_SENTINEL\"}")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    assert!(!malformed.text().await.unwrap().contains("SECRET_SENTINEL"));
    let missing = http
        .put(format!("{base}/providers/credential"))
        .bearer_auth("client")
        .json(&serde_json::json!({"provider":"Openrouter"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
    assert!(providers.provider(ProviderType::Openrouter).is_some());
    let status = client
        .set_provider_credential(&SetProviderCredential {
            provider: ProviderType::Openrouter,
            credential: None,
        })
        .await
        .unwrap();
    assert!(!status.authed);
    assert!(providers.provider(ProviderType::Openrouter).is_none());
    assert!(
        !client
            .list_providers()
            .await
            .unwrap()
            .iter()
            .find(|p| p.id == ProviderType::Openrouter)
            .unwrap()
            .authed
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
