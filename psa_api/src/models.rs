//! Provider discovery and the server-wide model catalog.
use llm_api::providers::{Effort, Model, ProviderType};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: ProviderType,
    pub name: String,
    /// Credentials are configured; this does not verify validity or entitlement.
    pub authed: bool,
}

/// Catalog reference prices in USD per million tokens, not subscription charges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCostRates {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    #[serde(flatten)]
    pub rates: ModelCostRates,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub tiers: Vec<ModelCostTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningOption {
    pub effort: Effort,
    /// None means omit reasoning parameters (for non-reasoning models).
    pub wire_effort: Option<Effort>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderType,
    pub model: Model,
    pub name: String,
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub cost: ModelCost,
    pub reasoning: bool,
    pub reasoning_options: Vec<ReasoningOption>,
    pub authed: bool,
    pub pinned: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub refreshed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListModels {
    pub provider: Option<ProviderType>,
    pub authed: Option<bool>,
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLookup {
    pub provider: ProviderType,
    /// Provider-specific API name; passed as a query parameter to support slashes.
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinModel {
    pub provider: ProviderType,
    pub model: Model,
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResyncModels {
    pub provider: Option<ProviderType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSyncResult {
    pub provider: ProviderType,
    pub success: bool,
    pub error: Option<String>,
}
