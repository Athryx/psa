//! Credentials never leave this server-side registry.

use llm_api::{
    api::{ApiProvider, Provider},
    providers::ProviderType,
};
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Vec<Provider>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Provider>) -> Self {
        Self { providers }
    }

    pub fn provider(&self, kind: ProviderType) -> Option<Provider> {
        self.providers
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

        let auth: Auth = toml::from_str(&contents).map_err(|_| AuthError::Invalid)?;
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

        Ok(Self { providers })
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_is_optional_typed_and_errors_never_contain_source_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        assert!(
            ProviderRegistry::load(&path)
                .await
                .unwrap()
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
