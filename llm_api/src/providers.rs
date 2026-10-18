use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderType {
    OpenAiCodex,
    Openrouter,
}

impl ProviderType {
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
/// `gpt-5`). The provider is tracked separately on `ModelConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub api_name: String,
}

impl Model {
    /// Construct a model from its API name, validating that the name is
    /// nonempty and has no surrounding whitespace.
    pub fn new(api_name: impl Into<String>) -> Result<Self, crate::Error> {
        let model = Self {
            api_name: api_name.into(),
        };

        model.validate()?;
        Ok(model)
    }

    /// Validate a nonempty API model name with no surrounding whitespace.
    fn validate(&self) -> Result<(), crate::Error> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
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
    pub provider: ProviderType,
    pub effort: Effort,
    // could add other configs like temperature top_p etc
    // but in modern llm, they often trained for specific defaults
    // and this can mess them up changing it
    // TODO: openrouter pinning stuff (decide how to represent well)
}

#[cfg(test)]
mod tests {
    use super::{Effort, Model, ProviderType};

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
            let model = Model::new(api_name).unwrap();
            assert_eq!(model.prefix(), expected_prefix);
            assert_eq!(model.model_name(), expected_name);
        }
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
