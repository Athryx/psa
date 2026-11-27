use super::*;
use crate::codex_auth::tests::build_jwt;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct FakeIssuer {
    refreshes: AtomicUsize,
    polls: AtomicUsize,
    reject_refresh: std::sync::atomic::AtomicBool,
}

fn issued_tokens(refresh_token: &str) -> Value {
    let exp = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    json!({
        "access_token": build_jwt(json!({"exp": exp})),
        "id_token": build_jwt(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "account-1"}
        })),
        "refresh_token": refresh_token,
    })
}

async fn start_issuer() -> (String, Arc<FakeIssuer>) {
    let fake = Arc::new(FakeIssuer::default());
    let app = Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(|| async {
                Json(json!({"device_auth_id": "device", "usercode": "ABCD-1234", "interval": "1"}))
            }),
        )
        .route(
            "/api/accounts/deviceauth/token",
            post(
                |State(fake): State<Arc<FakeIssuer>>, Json(body): Json<Value>| async move {
                    assert_eq!(body["device_auth_id"], "device");
                    if fake.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(StatusCode::NOT_FOUND);
                    }
                    Ok(Json(json!({
                        "authorization_code": "code",
                        "code_challenge": "challenge",
                        "code_verifier": "verifier",
                    })))
                },
            ),
        )
        .route(
            "/oauth/token",
            post(
                |State(fake): State<Arc<FakeIssuer>>, body: String| async move {
                    if let Ok(Json(body)) = Json::<Value>::from_bytes(body.as_bytes()) {
                        assert_eq!(body["grant_type"], "refresh_token");
                        assert_eq!(body["refresh_token"], "refresh-1");
                        fake.refreshes.fetch_add(1, Ordering::SeqCst);
                        if fake.reject_refresh.load(Ordering::SeqCst) {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                        // Give concurrent callers time to pile up behind the lock.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        return Ok(Json(issued_tokens("refresh-2")));
                    }
                    let form: HashMap<&str, &str> = body
                        .split('&')
                        .filter_map(|pair| pair.split_once('='))
                        .collect();
                    assert_eq!(form["grant_type"], "authorization_code");
                    assert_eq!(form["code"], "code");
                    assert_eq!(form["code_verifier"], "verifier");
                    Ok(Json(issued_tokens("refresh-login")))
                },
            ),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), fake)
}

async fn registry_with(
    contents: &str,
    issuer: &str,
) -> (tempfile::TempDir, PathBuf, ProviderRegistry) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.toml");
    tokio::fs::write(&path, contents).await.unwrap();
    let registry = ProviderRegistry::load(&path)
        .await
        .unwrap()
        .with_auth_path(path.clone())
        .with_codex_issuer(issuer);
    (dir, path, registry)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiring_tokens_refresh_once_and_are_saved() {
    let (issuer, fake) = start_issuer().await;
    let expired = OffsetDateTime::now_utc().unix_timestamp() - 10;
    let (_dir, path, registry) = registry_with(
        &format!("[openai_codex]\naccess_token = 'old'\nrefresh_token = 'refresh-1'\nexpires_at = {expired}\n"),
        &issuer,
    )
    .await;
    assert!(registry.info(ProviderType::OpenAiCodex).signed_in);

    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let registry = registry.clone();
        calls.spawn(async move { registry.ensure_provider(ProviderType::OpenAiCodex).await });
    }
    while let Some(result) = calls.join_next().await {
        assert!(result.unwrap().unwrap().is_some());
    }
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);

    let saved =
        auth::parse_provider_credentials(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
    let (_, credential) = &saved[0];
    assert_eq!(credential.refresh_token.as_deref(), Some("refresh-2"));
    assert_eq!(credential.account_id.as_deref(), Some("account-1"));
    assert!(!codex_auth::needs_refresh(credential, REFRESH_MARGIN));
}

#[tokio::test]
async fn rejected_refresh_fails_only_once_the_token_has_expired() {
    let (issuer, fake) = start_issuer().await;
    fake.reject_refresh.store(true, Ordering::SeqCst);
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for (expires_at, usable) in [(now + 60, true), (now - 60, false)] {
        let (_dir, _path, registry) = registry_with(
            &format!("[openai_codex]\naccess_token = 'old'\nrefresh_token = 'refresh-1'\nexpires_at = {expires_at}\n"),
            &issuer,
        )
        .await;
        let result = registry.ensure_provider(ProviderType::OpenAiCodex).await;
        assert_eq!(result.is_ok(), usable);
        if let Err(error) = result {
            assert!(matches!(error, AuthError::OAuth(OAuthError::Rejected)));
        }
    }
}

#[tokio::test]
async fn device_sign_in_saves_the_credential() {
    let (issuer, fake) = start_issuer().await;
    let (_dir, path, registry) =
        registry_with("# keep\n[openrouter]\napi_key = 'key'\n", &issuer).await;

    let login = registry
        .start_login(ProviderType::OpenAiCodex)
        .await
        .unwrap();
    assert_eq!(login.user_code, "ABCD-1234");
    assert_eq!(login.verification_url, format!("{issuer}/codex/device"));
    assert_eq!(login.status, ProviderLoginStatus::Pending);

    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = registry.login(login.id).unwrap().status;
            if status != ProviderLoginStatus::Pending {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status, ProviderLoginStatus::Complete);
    assert_eq!(fake.polls.load(Ordering::SeqCst), 2);

    let info = registry.info(ProviderType::OpenAiCodex);
    assert!(info.authed && info.signed_in);
    let contents = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(contents.starts_with("# keep\n[openrouter]\napi_key = 'key'\n"));
    assert!(contents.contains("refresh_token = \"refresh-login\""));

    assert!(matches!(
        registry.start_login(ProviderType::Openrouter).await,
        Err(AuthError::LoginUnsupported)
    ));
}

#[tokio::test]
async fn cancelled_sign_in_stops_polling() {
    let (issuer, fake) = start_issuer().await;
    let (_dir, path, registry) = registry_with("", &issuer).await;
    let login = registry
        .start_login(ProviderType::OpenAiCodex)
        .await
        .unwrap();

    let cancelled = registry.cancel_login(login.id).unwrap();
    assert!(matches!(
        cancelled.status,
        ProviderLoginStatus::Failed { .. }
    ));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.polls.load(Ordering::SeqCst), 0);
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "");
    assert!(registry.cancel_login(uuid::Uuid::new_v4()).is_none());
}
