use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderType {
    OpenAiCodex,
    Openrouter,
}

impl ProviderType {
    pub const ALL: [Self; 2] = [Self::OpenAiCodex, Self::Openrouter];

    /// Human-readable provider name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "OpenAI Codex",
            Self::Openrouter => "OpenRouter",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "openai_codex",
            Self::Openrouter => "openrouter",
        }
    }

    /// API root, without the operation-specific `/responses` or
    /// `/chat/completions` path. Codex uses the ChatGPT subscription endpoint,
    /// not the separately billed OpenAI platform API.
    pub const fn api_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "https://chatgpt.com/backend-api/codex",
            Self::Openrouter => "https://openrouter.ai/api/v1",
        }
    }
}

/// Model identified by its provider-specific API name, without any harness
/// provider prefix. On OpenRouter this is the `maker/model` slug the API
/// expects (e.g. `z-ai/glm-5.3`); Codex uses a plain model name (e.g.
/// `gpt-5`). The provider and API name together define model identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Model {
    pub provider: ProviderType,
    pub api_name: String,
}

impl Model {
    /// Construct a model from its provider and API name, validating that the
    /// name is nonempty and has no surrounding whitespace.
    pub fn new(provider: ProviderType, api_name: impl Into<String>) -> Result<Self, crate::Error> {
        let model = Self {
            provider,
            api_name: api_name.into(),
        };

        model.validate()?;
        Ok(model)
    }

    /// Parse a configuration slug in `provider_slug/api_name` form.
    ///
    /// The provider slug must match [`ProviderType::as_str`]. Only the first
    /// slash separates the provider, preserving API names such as `vendor/model`.
    /// Returns [`crate::Error::InvalidConfig`] for a missing or unknown provider,
    /// a missing separator, an empty model name, or surrounding whitespace.
    pub fn parse_slug(value: &str) -> Result<Self, crate::Error> {
        if value.trim() != value {
            return Err(crate::Error::InvalidConfig {
                field: "model".into(),
                message: "model slug must have no surrounding whitespace".into(),
            });
        }

        let (provider_slug, api_name) =
            value
                .split_once('/')
                .ok_or_else(|| crate::Error::InvalidConfig {
                    field: "model".into(),
                    message: "model slug must use provider_slug/api_name format".into(),
                })?;
        let provider = ProviderType::ALL
            .into_iter()
            .find(|provider| provider.as_str() == provider_slug)
            .ok_or_else(|| crate::Error::InvalidConfig {
                field: "model".into(),
                message: format!("missing or unknown model provider: {provider_slug:?}"),
            })?;

        Self::new(provider, api_name)
    }

    /// Validate a nonempty API model name with no surrounding whitespace.
    pub(crate) fn validate(&self) -> Result<(), crate::Error> {
        let name = self.api_name.as_str();

        if name.trim().is_empty() || name.trim() != name {
            return Err(crate::Error::InvalidConfig {
                field: "model.api_name".into(),
                message: "model name must be nonempty and have no surrounding whitespace".into(),
            });
        }

        Ok(())
    }

    /// Provider/publisher prefix, if present. For `z-ai/glm-5.3` this is
    /// `z-ai`; for an unprefixed `gpt-5` this is `None`.
    pub fn prefix(&self) -> Option<&str> {
        self.api_name.split_once('/').map(|(prefix, _)| prefix)
    }

    /// Bare model name, without the provider/publisher prefix. For
    /// `z-ai/glm-5.3` this is `glm-5.3`; for `gpt-5` it is `gpt-5`.
    pub fn model_name(&self) -> &str {
        self.api_name
            .split_once('/')
            .map_or(self.api_name.as_str(), |(_, name)| name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Effort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model: Model,
    pub effort: Effort,
    // could add other configs like temperature top_p etc
    // but in modern llm, they often trained for specific defaults
    // and this can mess them up changing it
    // TODO: openrouter pinning stuff (decide how to represent well)
}

#[cfg(test)]
mod tests {
    use super::{Effort, Model, ModelConfig, ProviderType};

    #[test]
    fn provider_names_are_human_readable() {
        assert_eq!(ProviderType::OpenAiCodex.name(), "OpenAI Codex");
        assert_eq!(ProviderType::Openrouter.name(), "OpenRouter");
    }

    #[test]
    fn api_base_urls_exclude_operation_paths() {
        assert_eq!(
            ProviderType::OpenAiCodex.api_base_url(),
            "https://chatgpt.com/backend-api/codex"
        );
        assert_eq!(
            ProviderType::Openrouter.api_base_url(),
            "https://openrouter.ai/api/v1"
        );
    }

    #[test]
    fn prefix_and_model_name_split_only_the_vendor_prefix() {
        for (api_name, expected_prefix, expected_name) in [
            ("gpt-5", None, "gpt-5"),
            ("z-ai/glm-5.3", Some("z-ai"), "glm-5.3"),
            (
                "anthropic/claude-sonnet-4",
                Some("anthropic"),
                "claude-sonnet-4",
            ),
        ] {
            let model = Model::new(ProviderType::Openrouter, api_name).unwrap();
            assert_eq!(model.prefix(), expected_prefix);
            assert_eq!(model.model_name(), expected_name);
        }
    }

    #[test]
    fn parse_slug_resolves_providers_and_preserves_api_names() {
        for (slug, provider, api_name) in [
            ("openai_codex/gpt-5", ProviderType::OpenAiCodex, "gpt-5"),
            ("openrouter/model", ProviderType::Openrouter, "model"),
            (
                "openrouter/vendor/model",
                ProviderType::Openrouter,
                "vendor/model",
            ),
            (
                "openai_codex/vendor/model/variant",
                ProviderType::OpenAiCodex,
                "vendor/model/variant",
            ),
        ] {
            assert_eq!(
                Model::parse_slug(slug).unwrap(),
                Model::new(provider, api_name).unwrap(),
            );
        }
    }

    #[test]
    fn parse_slug_rejects_malformed_slugs() {
        for slug in [
            "",
            "model",
            "openrouter",
            "/model",
            "unknown/model",
            "Openrouter/model",
            "openrouter/",
            "openai_codex/",
            "openrouter/ ",
            "openrouter/ model",
            "openrouter/model ",
            " openrouter/model",
            "openrouter /model",
            "openrouter/model\n",
            "\topenai_codex/gpt-5",
        ] {
            assert!(
                matches!(
                    Model::parse_slug(slug),
                    Err(crate::Error::InvalidConfig { .. })
                ),
                "expected invalid configuration for {slug:?}",
            );
        }
    }

    #[test]
    fn model_identity_includes_provider_and_api_name() {
        let model = Model::new(ProviderType::OpenAiCodex, "shared-model").unwrap();
        let same = Model::new(ProviderType::OpenAiCodex, "shared-model").unwrap();
        let other_provider = Model::new(ProviderType::Openrouter, "shared-model").unwrap();
        let other_name = Model::new(ProviderType::OpenAiCodex, "other-model").unwrap();

        assert_eq!(model, same);
        assert_ne!(model, other_provider);
        assert_ne!(model, other_name);
        let identities = std::collections::HashSet::from([model, same, other_provider, other_name]);
        assert_eq!(identities.len(), 3);
    }

    #[test]
    fn model_config_serializes_provider_inside_model() {
        let config = ModelConfig {
            model: Model::new(ProviderType::Openrouter, "vendor/model").unwrap(),
            effort: Effort::High,
        };
        let expected = serde_json::json!({
            "model": {"provider": "Openrouter", "api_name": "vendor/model"},
            "effort": "High",
        });

        assert_eq!(serde_json::to_value(&config).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<ModelConfig>(expected).unwrap(),
            config
        );
    }

    #[test]
    fn xhigh_keeps_its_api_spelling_and_reads_the_old_variant() {
        assert_eq!(Effort::Xhigh.as_str(), "xhigh");
        assert_eq!(
            serde_json::from_str::<Effort>("\"Xhigh\"").unwrap(),
            Effort::Xhigh
        );
        assert_eq!(serde_json::to_string(&Effort::Xhigh).unwrap(), "\"Xhigh\"");
    }
}
