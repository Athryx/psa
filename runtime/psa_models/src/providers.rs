//! Credentials never leave this server-side registry.

use crate::codex_auth::{self, CodexOAuth, DeviceCode, OAuthError};
use llm_api::{
    api::{ApiProvider, Auth, Provider, decisions::Decisions},
    providers::ProviderType,
};
use psa::api::{ProviderInfo, ProviderLogin, ProviderLoginStatus};
use psa_config::{
    auth::{self, AuthFileError, ProviderCredential},
    server::DEFAULT_MAX_CONCURRENT_DECISIONS,
};
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use time::OffsetDateTime;

/// Refresh OAuth access tokens this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Finished sign-ins stay visible to pollers this long.
const FINISHED_LOGIN_RETENTION: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    snapshot: Arc<RwLock<Snapshot>>,
    /// Per-session concurrency cap; `None` uses the built-in default.
    max_concurrent_decisions: Option<NonZeroUsize>,
    auth_path: Option<PathBuf>,
    catalog: Option<Box<crate::catalog::ModelCatalog>>,
    catalog_source: Option<crate::catalog::CatalogSource>,
    oauth: Option<CodexOAuth>,
    /// Serializes credential writes. Refresh tokens are single-use, so two
    /// concurrent refreshes would sign the server out.
    writes: Arc<tokio::sync::Mutex<()>>,
    logins: Arc<Mutex<HashMap<uuid::Uuid, LoginEntry>>>,
}

/// Provider clients and the credentials they were built from, replaced together.
#[derive(Clone, Default)]
struct Snapshot {
    providers: Vec<Provider>,
    /// Decision-model clients for providers that offer them.
    decisions: Vec<Decisions>,
    credentials: Vec<(ProviderType, ProviderCredential)>,
}

struct LoginEntry {
    login: ProviderLogin,
    finished_at: Option<OffsetDateTime>,
    poller: Option<tokio::task::AbortHandle>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Provider>) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(Snapshot {
                providers,
                ..Snapshot::default()
            })),
            ..Self::default()
        }
    }

    /// Replace the decision-model clients, for registries built without credentials files.
    pub fn with_decisions(self, decisions: Vec<Decisions>) -> Self {
        self.snapshot
            .write()
            .expect("provider registry lock poisoned")
            .decisions = decisions;
        self
    }

    pub fn with_max_concurrent_decisions(mut self, limit: NonZeroUsize) -> Self {
        self.max_concurrent_decisions = Some(limit);
        self
    }

    /// Decision calls a session may run at once; further calls queue.
    pub fn max_concurrent_decisions(&self) -> NonZeroUsize {
        self.max_concurrent_decisions
            .unwrap_or(DEFAULT_MAX_CONCURRENT_DECISIONS)
    }

    pub fn with_catalog_source(mut self, source: crate::catalog::CatalogSource) -> Self {
        self.catalog_source = Some(source);
        self
    }

    pub fn catalog_source(&self) -> crate::catalog::CatalogSource {
        self.catalog_source
            .clone()
            .unwrap_or_else(crate::catalog::CatalogSource::pi)
    }

    pub fn with_catalog(mut self, catalog: crate::catalog::ModelCatalog) -> Self {
        self.catalog = Some(Box::new(catalog));
        self
    }

    pub fn catalog(&self) -> Option<&crate::catalog::ModelCatalog> {
        self.catalog.as_deref()
    }

    /// Use another ChatGPT OAuth issuer, for tests.
    pub fn with_codex_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.oauth = Some(CodexOAuth::new(issuer));
        self
    }

    pub async fn resolve_reasoning(
        &self,
        config: &llm_api::providers::ModelConfig,
    ) -> Result<Option<llm_api::api::ReasoningOverride>, llm_api::Error> {
        let Some(catalog) = &self.catalog else {
            return Ok(None);
        };
        catalog
            .resolve_reasoning(config)
            .await
            .map_err(|error| llm_api::Error::InvalidRequest {
                message: error.to_string(),
            })
    }

    pub fn provider(&self, kind: ProviderType) -> Option<Provider> {
        self.read_snapshot()
            .providers
            .iter()
            .find(|provider| provider.provider_type() == kind)
            .cloned()
    }

    /// The provider client to send a request with, refreshing its OAuth
    /// token first when it is about to expire.
    pub async fn ensure_provider(&self, kind: ProviderType) -> Result<Option<Provider>, AuthError> {
        if self
            .credential(kind)
            .is_some_and(|c| codex_auth::needs_refresh(&c, REFRESH_MARGIN))
        {
            // Finish the refresh even if the caller is cancelled: the old
            // refresh token stops working once the new one is issued.
            let registry = self.clone();
            let refreshed = tokio::spawn(async move { registry.refresh_credential(kind).await })
                .await
                .map_err(|_| AuthError::Unavailable)?;
            if let Err(error) = refreshed {
                // A token that has not expired yet is still worth trying.
                if self
                    .credential(kind)
                    .is_none_or(|c| codex_auth::is_expired(&c))
                {
                    return Err(error);
                }
                tracing::warn!(provider = kind.as_str(), %error, "token refresh failed");
            }
        }
        Ok(self.provider(kind))
    }

    /// The decision-model client for a provider, when it has credentials and
    /// offers decision models.
    pub fn decisions(&self, kind: ProviderType) -> Option<Decisions> {
        self.read_snapshot()
            .decisions
            .iter()
            .find(|decisions| decisions.provider_type() == kind)
            .cloned()
    }

    /// Configuration status of one provider, without its credential.
    pub fn info(&self, kind: ProviderType) -> ProviderInfo {
        let credential = self.credential(kind);
        ProviderInfo {
            id: kind,
            authed: self.provider(kind).is_some(),
            signed_in: credential.is_some_and(|c| c.refresh_token.is_some()),
            supports_login: kind == ProviderType::OpenAiCodex,
        }
    }

    pub async fn load(path: &Path) -> Result<Self, AuthError> {
        let contents = match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(AuthError::Io(error)),
        };

        Ok(Self {
            snapshot: Arc::new(RwLock::new(parse_snapshot(&contents)?)),
            ..Self::default()
        })
    }

    pub fn with_auth_path(mut self, path: PathBuf) -> Self {
        self.auth_path = Some(path);
        self
    }

    /// Persist and publish a complete validated snapshot while holding the shared file lock.
    pub async fn set_credential(
        &self,
        request: psa::api::SetProviderCredential,
    ) -> Result<ProviderInfo, AuthError> {
        if let Some(credential) = &request.credential {
            if credential.trim().is_empty()
                || Provider::new(request.provider, credential.as_str()).is_err()
            {
                return Err(AuthError::InvalidCredential);
            }
        }
        let credential = request.credential.map(ProviderCredential::token);

        let _write = self.writes.lock().await;
        self.write_credential(request.provider, credential.as_ref())
            .await?;
        Ok(self.info(request.provider))
    }

    /// Begin a device-code sign-in. The server polls for approval in the
    /// background and saves the credential when the user approves.
    pub async fn start_login(&self, kind: ProviderType) -> Result<ProviderLogin, AuthError> {
        if kind != ProviderType::OpenAiCodex {
            return Err(AuthError::LoginUnsupported);
        }
        self.auth_path.as_ref().ok_or(AuthError::Unavailable)?;
        let code = self.oauth().request_device_code().await?;

        let login = ProviderLogin {
            id: uuid::Uuid::new_v4(),
            provider: kind,
            verification_url: code.verification_url.clone(),
            user_code: code.user_code.clone(),
            expires_at: OffsetDateTime::now_utc() + codex_auth::DEVICE_CODE_LIFETIME,
            status: ProviderLoginStatus::Pending,
        };
        let registry = self.clone();
        let id = login.id;
        let poller = tokio::spawn(async move {
            let status = registry.poll_login(kind, &code).await;
            registry.finish_login(id, status);
        });

        let mut logins = self.logins.lock().expect("login lock poisoned");
        let now = OffsetDateTime::now_utc();
        logins.retain(|_, entry| {
            entry
                .finished_at
                .is_none_or(|finished| now - finished < FINISHED_LOGIN_RETENTION)
        });
        logins.insert(
            id,
            LoginEntry {
                login: login.clone(),
                finished_at: None,
                poller: Some(poller.abort_handle()),
            },
        );
        Ok(login)
    }

    pub fn login(&self, id: uuid::Uuid) -> Option<ProviderLogin> {
        let logins = self.logins.lock().expect("login lock poisoned");
        logins.get(&id).map(|entry| entry.login.clone())
    }

    /// Stop waiting for a pending sign-in. Finished sign-ins are unchanged.
    pub fn cancel_login(&self, id: uuid::Uuid) -> Option<ProviderLogin> {
        let mut logins = self.logins.lock().expect("login lock poisoned");
        let entry = logins.get_mut(&id)?;
        if let Some(poller) = entry.poller.take() {
            poller.abort();
            entry.login.status = ProviderLoginStatus::Failed {
                message: "sign-in cancelled".into(),
            };
            entry.finished_at = Some(OffsetDateTime::now_utc());
        }
        Some(entry.login.clone())
    }

    async fn poll_login(&self, kind: ProviderType, code: &DeviceCode) -> ProviderLoginStatus {
        let deadline = tokio::time::Instant::now() + codex_auth::DEVICE_CODE_LIFETIME;
        let credential = loop {
            tokio::time::sleep(code.interval).await;
            if tokio::time::Instant::now() >= deadline {
                return failed("the code expired; start a new sign-in");
            }
            match self.oauth().poll_device_code(code).await {
                Ok(None) => continue,
                Ok(Some(credential)) => break credential,
                Err(error) => return failed(&error.to_string()),
            }
        };

        // Save in its own task so cancelling the sign-in cannot interrupt a write.
        let registry = self.clone();
        let saved = tokio::spawn(async move {
            let _write = registry.writes.lock().await;
            registry.write_credential(kind, Some(&credential)).await
        })
        .await;
        match saved {
            Ok(Ok(())) => ProviderLoginStatus::Complete,
            Ok(Err(error)) => failed(&error.to_string()),
            Err(_) => failed("could not save the credential"),
        }
    }

    fn finish_login(&self, id: uuid::Uuid, status: ProviderLoginStatus) {
        let mut logins = self.logins.lock().expect("login lock poisoned");
        let Some(entry) = logins.get_mut(&id) else {
            return;
        };
        entry.poller = None;
        entry.login.status = status;
        entry.finished_at = Some(OffsetDateTime::now_utc());
    }

    /// Exchange the saved refresh token and store the new token set.
    async fn refresh_credential(&self, kind: ProviderType) -> Result<(), AuthError> {
        let _write = self.writes.lock().await;
        // Another caller may have refreshed while this one waited.
        let Some(current) = self.credential(kind) else {
            return Ok(());
        };
        if !codex_auth::needs_refresh(&current, REFRESH_MARGIN) {
            return Ok(());
        }

        let refreshed = self.oauth().refresh(&current).await?;
        self.write_credential(kind, Some(&refreshed)).await
    }

    /// Save one provider's credential and publish clients built from the
    /// whole file. Callers hold `writes`. A malformed file is never repaired
    /// or overwritten.
    async fn write_credential(
        &self,
        kind: ProviderType,
        credential: Option<&ProviderCredential>,
    ) -> Result<(), AuthError> {
        let path = self.auth_path.as_ref().ok_or(AuthError::Unavailable)?;
        let _lock = psa_shared::local_config::lock(path).await?;
        let text = auth::read_optional(path).await?;
        parse_snapshot(&text)?;
        let mut document = auth::parse_document(&text)?;
        auth::validate_existing_keys(&document)?;

        auth::apply_provider_credential(&mut document, kind, credential);
        let updated = parse_snapshot(&document.to_string())?;
        auth::write_document(path, &document).await?;
        *self
            .snapshot
            .write()
            .expect("provider registry lock poisoned") = updated;
        Ok(())
    }

    fn credential(&self, kind: ProviderType) -> Option<ProviderCredential> {
        self.read_snapshot()
            .credentials
            .iter()
            .find(|(provider, _)| *provider == kind)
            .map(|(_, credential)| credential.clone())
    }

    fn oauth(&self) -> CodexOAuth {
        self.oauth
            .clone()
            .unwrap_or_else(|| CodexOAuth::new(codex_auth::DEFAULT_ISSUER))
    }

    fn read_snapshot(&self) -> std::sync::RwLockReadGuard<'_, Snapshot> {
        self.snapshot
            .read()
            .expect("provider registry lock poisoned")
    }
}

fn failed(message: &str) -> ProviderLoginStatus {
    ProviderLoginStatus::Failed {
        message: message.to_owned(),
    }
}

fn parse_snapshot(contents: &str) -> Result<Snapshot, AuthError> {
    let mut snapshot = Snapshot::default();
    for (kind, credential) in auth::parse_provider_credentials(contents)? {
        let auth = build_auth(kind, &credential);
        if kind.decisions_url().is_some() {
            snapshot
                .decisions
                .push(Decisions::new(kind, auth.clone()).map_err(|_| AuthError::Invalid)?);
        }
        snapshot
            .providers
            .push(Provider::new(kind, auth).map_err(|_| AuthError::Invalid)?);
        snapshot.credentials.push((kind, credential));
    }
    Ok(snapshot)
}

/// Request headers for a stored credential.
fn build_auth(kind: ProviderType, credential: &ProviderCredential) -> Auth {
    let auth = Auth::bearer(credential.token.as_str());
    match (kind, &credential.account_id) {
        (ProviderType::OpenAiCodex, Some(account_id)) => {
            auth.with_header("chatgpt-account-id", account_id.as_str())
        }
        _ => auth,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("cannot read provider auth file: {0}")]
    Io(#[from] std::io::Error),
    // Do not retain TOML errors: their source excerpts can contain credentials.
    #[error("invalid auth.toml or provider credentials")]
    Invalid,
    #[error("invalid provider credential")]
    InvalidCredential,
    #[error("provider configuration is unavailable")]
    Unavailable,
    #[error("this provider does not support signing in")]
    LoginUnsupported,
    #[error("{0}")]
    OAuth(#[from] OAuthError),
}

impl From<AuthFileError> for AuthError {
    fn from(error: AuthFileError) -> Self {
        match error {
            AuthFileError::Io(error) => Self::Io(error),
            AuthFileError::Invalid => Self::Invalid,
        }
    }
}

#[cfg(test)]
#[path = "tests/providers/oauth_tests.rs"]
mod oauth_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn updates_preserve_file_and_are_shared_and_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        tokio::fs::write(&path, "# keep comment\n[psa]\nclient_key = 'client'\nprobe_key = 'probe'\n\n[openai_codex]\naccess_token = 'codex'\n").await.unwrap();
        let registry = ProviderRegistry::load(&path)
            .await
            .unwrap()
            .with_auth_path(path.clone());
        let other = registry.clone();
        let set = |provider, credential: Option<&str>| psa::api::SetProviderCredential {
            provider,
            credential: credential.map(str::to_owned),
        };
        registry
            .set_credential(set(ProviderType::Openrouter, Some("SECRET_SENTINEL")))
            .await
            .unwrap();
        assert!(other.provider(ProviderType::Openrouter).is_some());
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.contains("# keep comment"));
        assert!(contents.contains("client_key = 'client'"));
        assert!(contents.contains("access_token = 'codex'"));
        assert!(
            ProviderRegistry::load(&path)
                .await
                .unwrap()
                .provider(ProviderType::Openrouter)
                .is_some()
        );
        let error = registry
            .set_credential(set(ProviderType::Openrouter, Some("\nSECRET_SENTINEL")))
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
        assert!(other.provider(ProviderType::Openrouter).is_some());
        registry
            .set_credential(set(ProviderType::Openrouter, None))
            .await
            .unwrap();
        assert!(other.provider(ProviderType::Openrouter).is_none());
        assert!(other.provider(ProviderType::OpenAiCodex).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&path)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn failed_update_leaves_memory_and_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        let malformed = "[psa]\nclient_key = ''\n";
        tokio::fs::write(&path, malformed).await.unwrap();
        let registry = ProviderRegistry::new(vec![
            Provider::new(ProviderType::Openrouter, "original").unwrap(),
        ])
        .with_auth_path(path.clone());
        let request = psa::api::SetProviderCredential {
            provider: ProviderType::Openrouter,
            credential: None,
        };
        assert!(registry.set_credential(request.clone()).await.is_err());
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), malformed);
        assert!(registry.provider(ProviderType::Openrouter).is_some());
        let missing_parent = registry
            .clone()
            .with_auth_path(dir.path().join("missing/auth.toml"));
        assert!(missing_parent.set_credential(request).await.is_err());
        assert!(registry.provider(ProviderType::Openrouter).is_some());
    }

    #[tokio::test]
    async fn concurrent_updates_do_not_lose_other_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        let registry = ProviderRegistry::default().with_auth_path(path.clone());
        let set = |provider| psa::api::SetProviderCredential {
            provider,
            credential: Some("valid-key".into()),
        };
        let (a, b) = tokio::join!(
            registry.set_credential(set(ProviderType::Openrouter)),
            registry.set_credential(set(ProviderType::OpenAiCodex))
        );
        a.unwrap();
        b.unwrap();
        let reloaded = ProviderRegistry::load(&path).await.unwrap();
        for kind in ProviderType::ALL {
            assert!(registry.provider(kind).is_some());
            assert!(reloaded.provider(kind).is_some());
        }
    }

    #[tokio::test]
    async fn auth_is_optional_typed_and_errors_never_contain_source_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        assert!(
            ProviderRegistry::load(&path)
                .await
                .unwrap()
                .read_snapshot()
                .providers
                .is_empty()
        );
        tokio::fs::write(&path, "[openrouter]\napi_key = 'test-key'\n")
            .await
            .unwrap();
        assert!(
            ProviderRegistry::load(&path)
                .await
                .unwrap()
                .provider(ProviderType::Openrouter)
                .is_some()
        );
        assert!(
            ProviderRegistry::load(&path)
                .await
                .unwrap()
                .provider(ProviderType::OpenAiCodex)
                .is_none()
        );
        for invalid in [
            "[openrouter]\napi_key = 'SECRET_SENTINEL'\nunexpected = true",
            "[openrouter]\napi_key = \"SECRET_SENTINEL",
            "[openrouter]\napi_key = ''",
        ] {
            tokio::fs::write(&path, invalid).await.unwrap();
            let Err(error) = ProviderRegistry::load(&path).await else {
                panic!("invalid auth accepted");
            };
            assert!(!format!("{error:?} {error}").contains("SECRET_SENTINEL"));
        }
    }
}
