use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderType {
    OpenAiCodex,
    Openrouter,
}

/// Model with model slug, includes provider prefix with slashes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub model_slug: String,
}

impl Model {
    /// Strip the harness provider prefix, preserving the remaining model path.
    /// For example, `openrouter/anthropic/claude-sonnet-4` becomes
    /// `anthropic/claude-sonnet-4`. Unprefixed names are returned unchanged.
    pub fn api_name(&self) -> String {
        self.model_slug
            .split_once('/')
            .map_or(self.model_slug.as_str(), |(_, name)| name)
            .to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    Low,
    Medium,
    High,
    #[serde(alias = "Xhixh")]
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

impl ModelConfig {
    /// Validate the harness provider prefix and nonempty API model name.
    pub fn validate(&self) -> Result<(), crate::Error> {
        let slug = self.model.model_slug.as_str();
        if slug.trim().is_empty() || slug.trim() != slug {
            return Err(crate::Error::InvalidConfig {
                field: "model.model_slug".into(),
                message: "model name must be nonempty and have no surrounding whitespace".into(),
            });
        }
        if let Some((prefix, name)) = slug.split_once('/') {
            let actual =
                match prefix {
                    "openai-codex" => ProviderType::OpenAiCodex,
                    "openrouter" => ProviderType::Openrouter,
                    _ => return Err(crate::Error::InvalidConfig {
                        field: "model.model_slug".into(),
                        message:
                            "slash-containing names must start with openai-codex/ or openrouter/"
                                .into(),
                    }),
                };
            if actual != self.provider {
                return Err(crate::Error::ProviderMismatch {
                    expected: self.provider,
                    actual,
                });
            }
            if name.trim().is_empty() {
                return Err(crate::Error::InvalidConfig {
                    field: "model.model_slug".into(),
                    message: "API model name must be nonempty".into(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Effort, Model};

    #[test]
    fn api_name_strips_only_the_provider_prefix() {
        for (slug, expected) in [
            ("openai-codex/gpt-5", "gpt-5"),
            (
                "openrouter/anthropic/claude-sonnet-4",
                "anthropic/claude-sonnet-4",
            ),
            ("gpt-5", "gpt-5"),
        ] {
            let model = Model {
                model_slug: slug.into(),
            };
            assert_eq!(model.api_name(), expected);
        }
    }

    #[test]
    fn xhigh_keeps_its_api_spelling_and_reads_the_old_variant() {
        assert_eq!(Effort::Xhigh.as_str(), "xhigh");
        assert_eq!(
            serde_json::from_str::<Effort>("\"Xhixh\"").unwrap(),
            Effort::Xhigh
        );
        assert_eq!(serde_json::to_string(&Effort::Xhigh).unwrap(), "\"Xhigh\"");
    }
}
