//! Credentials never leave this server-side registry.

use llm_api::{
    api::{ApiProvider, Provider, decisions::Decisions},
    providers::ProviderType,
};
use psa_config::{
    auth::{self, AuthFileError},
    server::DEFAULT_MAX_CONCURRENT_DECISIONS,
};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Arc<RwLock<Vec<Provider>>>,
    /// Decision-model clients for providers that offer them. They hold the same
    /// credentials as `providers` and are replaced together with them.
    decisions: Arc<RwLock<Vec<Decisions>>>,
    /// Per-session concurrency cap; `None` uses the built-in default.
    max_concurrent_decisions: Option<NonZeroUsize>,
    auth_path: Option<PathBuf>,
    catalog: Option<Box<crate::catalog::ModelCatalog>>,
    catalog_source: Option<crate::catalog::CatalogSource>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Provider>) -> Self {
        Self {
            providers: Arc::new(RwLock::new(providers)),
            decisions: Arc::default(),
            max_concurrent_decisions: None,
            auth_path: None,
            catalog: None,
            catalog_source: None,
        }
    }

    /// Replace the decision-model clients, for registries built without credentials files.
    pub fn with_decisions(self, decisions: Vec<Decisions>) -> Self {
        *self
            .decisions
            .write()
            .expect("provider registry lock poisoned") = decisions;
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
        self.providers
            .read()
            .expect("provider registry lock poisoned")
            .iter()
            .find(|provider| provider.provider_type() == kind)
            .cloned()
    }

    /// The decision-model client for a provider, when it has credentials and
    /// offers decision models.
    pub fn decisions(&self, kind: ProviderType) -> Option<Decisions> {
        self.decisions
            .read()
            .expect("provider registry lock poisoned")
            .iter()
            .find(|decisions| decisions.provider_type() == kind)
            .cloned()
    }

    pub async fn load(path: &Path) -> Result<Self, AuthError> {
        let contents = match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(AuthError::Io(error)),
        };

        Self::parse(&contents)
    }

    fn parse(contents: &str) -> Result<Self, AuthError> {
        let mut providers = Vec::new();
        let mut decisions = Vec::new();
        for (kind, credential) in auth::parse_provider_credentials(contents)? {
            if kind.decisions_url().is_some() {
                decisions.push(
                    Decisions::new(kind, credential.clone()).map_err(|_| AuthError::Invalid)?,
                );
            }
            providers.push(Provider::new(kind, credential).map_err(|_| AuthError::Invalid)?);
        }

        Ok(Self::new(providers).with_decisions(decisions))
    }

    pub fn with_auth_path(mut self, path: PathBuf) -> Self {
        self.auth_path = Some(path);
        self
    }

    /// Persist and publish a complete validated snapshot while holding the shared file lock.
    pub async fn set_credential(
        &self,
        request: psa::api::SetProviderCredential,
    ) -> Result<psa::api::ProviderInfo, AuthError> {
        let path = self.auth_path.as_ref().ok_or(AuthError::Unavailable)?;
        if let Some(credential) = &request.credential {
            if credential.trim().is_empty()
                || Provider::new(request.provider, credential.clone()).is_err()
            {
                return Err(AuthError::InvalidCredential);
            }
        }
        let _lock = psa_shared::local_config::lock(path).await?;
        let text = auth::read_optional(path).await?;
        // Never repair or overwrite a malformed file.
        Self::parse(&text)?;
        let mut document = auth::parse_document(&text)?;
        auth::validate_existing_keys(&document)?;
        let (section, field) = auth::provider_credential_field(request.provider);
        if let Some(credential) = request.credential {
            if document.get(section).is_none() {
                document[section] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            document[section][field] = toml_edit::value(credential);
        } else {
            document.remove(section);
        }
        let updated = Self::parse(&document.to_string())?;
        auth::write_document(path, &document).await?;
        *self
            .providers
            .write()
            .expect("provider registry lock poisoned") = updated
            .providers
            .read()
            .expect("provider registry lock poisoned")
            .clone();
        *self
            .decisions
            .write()
            .expect("provider registry lock poisoned") = updated
            .decisions
            .read()
            .expect("provider registry lock poisoned")
            .clone();
        Ok(psa::api::ProviderInfo {
            id: request.provider,
            authed: self.provider(request.provider).is_some(),
        })
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
                .providers
                .read()
                .unwrap()
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
