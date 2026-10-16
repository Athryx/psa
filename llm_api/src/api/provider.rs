//! Enum dispatch over the supported API clients and their owned streams.

use crate::Error;
use crate::providers::ProviderType;

use super::{
    AgentHistory, ApiProvider, ModelTurnInput, ModelTurnOutput, ModelTurnStream, OpenAiCompletions,
    OpenAiCompletionsConfig, OpenAiCompletionsStream, OpenAiResponses, OpenAiResponsesStream,
    RequestConfig, TurnEvent,
};

/// A configured client without trait objects or boxed futures.
/// Variants may also be constructed directly to use custom endpoint URLs.
#[derive(Clone)]
pub enum Provider {
    Responses(OpenAiResponses),
    Completions(OpenAiCompletions),
}

impl Provider {
    /// Select the protocol, endpoint, and wire settings for a provider.
    /// Credentials are forwarded as bearer tokens; OAuth login/token refresh
    /// are not performed here. Codex needs a ChatGPT access token, not a platform
    /// API key. Its subscription-specific request/header compatibility is not
    /// implemented by the generic Responses client yet.
    pub fn new(provider: ProviderType, api_key: impl Into<String>) -> Result<Self, Error> {
        let api_key = api_key.into();
        let base_url = provider.api_base_url();
        match provider {
            ProviderType::OpenAiCodex => Ok(Self::Responses(OpenAiResponses::new(
                format!("{base_url}/responses"),
                api_key,
                provider,
            )?)),
            ProviderType::Openrouter => Ok(Self::Completions(OpenAiCompletions::new(
                format!("{base_url}/chat/completions"),
                api_key,
                provider,
                OpenAiCompletionsConfig {
                    openrouter_reasoning_fields: true,
                },
            )?)),
        }
    }
}

impl ApiProvider for Provider {
    type Stream = ProviderStream;

    fn provider_type(&self) -> ProviderType {
        match self {
            Self::Responses(provider) => provider.provider_type(),
            Self::Completions(provider) => provider.provider_type(),
        }
    }

    async fn generate_inner(
        &self,
        history: &AgentHistory,
        input: &ModelTurnInput,
        config: &RequestConfig,
    ) -> Result<Self::Stream, Error> {
        match self {
            Self::Responses(provider) => provider
                .generate_inner(history, input, config)
                .await
                .map(ProviderStream::Responses),
            Self::Completions(provider) => provider
                .generate_inner(history, input, config)
                .await
                .map(ProviderStream::Completions),
        }
    }
}

/// Preserves the concrete stream's polling, cancellation, and drop semantics.
pub enum ProviderStream {
    Responses(OpenAiResponsesStream),
    Completions(OpenAiCompletionsStream),
}

impl ModelTurnStream for ProviderStream {
    fn current(&self) -> &ModelTurnOutput {
        match self {
            Self::Responses(stream) => stream.current(),
            Self::Completions(stream) => stream.current(),
        }
    }

    async fn next_event(&mut self) -> Option<Result<TurnEvent, Error>> {
        match self {
            Self::Responses(stream) => stream.next_event().await,
            Self::Completions(stream) => stream.next_event().await,
        }
    }

    fn close(&mut self) {
        match self {
            Self::Responses(stream) => stream.close(),
            Self::Completions(stream) => stream.close(),
        }
    }

    fn into_output(self) -> ModelTurnOutput {
        match self {
            Self::Responses(stream) => stream.into_output(),
            Self::Completions(stream) => stream.into_output(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Effort, Model, ModelConfig};

    #[test]
    fn selects_client_and_preserves_identity_on_clone() {
        let codex = Provider::new(ProviderType::OpenAiCodex, "token").unwrap();
        assert!(matches!(codex, Provider::Responses(_)));
        assert_eq!(codex.clone().provider_type(), ProviderType::OpenAiCodex);
        let router = Provider::new(ProviderType::Openrouter, "key").unwrap();
        assert!(matches!(router, Provider::Completions(_)));
        assert_eq!(router.clone().provider_type(), ProviderType::Openrouter);
    }

    #[test]
    fn constructor_validates_credentials() {
        for provider in [ProviderType::OpenAiCodex, ProviderType::Openrouter] {
            assert!(matches!(
                Provider::new(provider, "invalid\nkey"),
                Err(Error::InvalidConfig { .. })
            ));
        }
    }

    #[tokio::test]
    async fn validates_history_before_delegating_to_http() {
        for provider in [ProviderType::OpenAiCodex, ProviderType::Openrouter] {
            let client = Provider::new(provider, "key").unwrap();
            let other = match provider {
                ProviderType::OpenAiCodex => ProviderType::Openrouter,
                ProviderType::Openrouter => ProviderType::OpenAiCodex,
            };
            let history = AgentHistory::new(ModelConfig {
                provider: other,
                model: Model {
                    model_slug: "test-model".into(),
                },
                effort: Effort::Medium,
            });
            assert!(
                matches!(client.generate(&history, &ModelTurnInput::default(), &RequestConfig::default()).await,
                Err(Error::ProviderMismatch { expected, actual }) if expected == provider && actual == other)
            );
        }
    }
}
