//! Credentials never leave this server-side registry.

use llm_api::{
    api::{ApiProvider, Provider},
    providers::ProviderType,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Arc<RwLock<Vec<Provider>>>,
    auth_path: Option<PathBuf>,
    catalog: Option<Box<crate::models::ModelCatalog>>,
    catalog_source: Option<crate::models::CatalogSource>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Provider>) -> Self {
        Self {
            providers: Arc::new(RwLock::new(providers)),
            auth_path: None,
            catalog: None,
            catalog_source: None,
        }
    }

    pub fn with_catalog_source(mut self, source: crate::models::CatalogSource) -> Self {
        self.catalog_source = Some(source);
        self
    }

    pub(crate) fn catalog_source(&self) -> crate::models::CatalogSource {
        self.catalog_source
            .clone()
            .unwrap_or_else(crate::models::CatalogSource::pi)
    }

    pub(crate) fn with_catalog(mut self, catalog: crate::models::ModelCatalog) -> Self {
        self.catalog = Some(Box::new(catalog));
        self
    }

    pub(crate) fn catalog(&self) -> Option<&crate::models::ModelCatalog> {
        self.catalog.as_deref()
    }

    pub(crate) async fn resolve_reasoning(
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
        let auth: Auth = toml::from_str(contents).map_err(|_| AuthError::Invalid)?;
        let _ = &auth.psa;
        let mut providers = Vec::new();
        for (kind, credential) in [
            (
                ProviderType::Openrouter,
                auth.openrouter.map(|auth| auth.api_key),
            ),
            (
                ProviderType::OpenAiCodex,
                auth.openai_codex.map(|auth| auth.access_token),
            ),
        ] {
            if let Some(credential) = credential {
                if credential.trim().is_empty() {
                    return Err(AuthError::Invalid);
                }
                providers.push(Provider::new(kind, credential).map_err(|_| AuthError::Invalid)?);
            }
        }

        Ok(Self::new(providers))
    }

    pub fn with_auth_path(mut self, path: PathBuf) -> Self {
        self.auth_path = Some(path);
        self
    }

    /// Persist and publish a complete validated snapshot while holding the shared file lock.
    pub async fn set_credential(
        &self,
        request: psa_api::SetProviderCredential,
    ) -> Result<psa_api::ProviderInfo, AuthError> {
        let path = self.auth_path.as_ref().ok_or(AuthError::Unavailable)?;
        if let Some(credential) = &request.credential {
            if credential.trim().is_empty()
                || Provider::new(request.provider, credential.clone()).is_err()
            {
                return Err(AuthError::InvalidCredential);
            }
        }
        let _lock = psa_shared::local_config::lock(path).await?;
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        // Never repair or overwrite a malformed file.
        Self::parse(&text)?;
        let mut document = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| AuthError::Invalid)?;
        crate::auth::validate_existing_keys(&document)?;
        let (section, field) = match request.provider {
            ProviderType::Openrouter => ("openrouter", "api_key"),
            ProviderType::OpenAiCodex => ("openai_codex", "access_token"),
        };
        if let Some(credential) = request.credential {
            if document.get(section).is_none() {
                document[section] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            document[section][field] = toml_edit::value(credential);
        } else {
            document.remove(section);
        }
        let updated = Self::parse(&document.to_string())?;
        crate::auth::write_document(path, &document).await?;
        *self
            .providers
            .write()
            .expect("provider registry lock poisoned") = updated
            .providers
            .read()
            .expect("provider registry lock poisoned")
            .clone();
        Ok(psa_api::ProviderInfo {
            id: request.provider,
            authed: self.provider(request.provider).is_some(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Auth {
    psa: Option<toml::Value>,
    openrouter: Option<OpenrouterAuth>,
    openai_codex: Option<CodexAuth>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenrouterAuth {
    api_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexAuth {
    access_token: String,
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
        let set = |provider, credential: Option<&str>| psa_api::SetProviderCredential {
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
        let request = psa_api::SetProviderCredential {
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
        let set = |provider| psa_api::SetProviderCredential {
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
