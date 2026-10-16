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
    /// Returns name we would submit to api
    pub fn api_name(&self) -> String {
        todo!()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhixh,
    Max,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhixh => "xhigh",
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
