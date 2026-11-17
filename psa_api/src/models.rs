//! Provider discovery and the server-wide model catalog.
use llm_api::providers::{Effort, Model, ProviderType};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Supported provider and its live credential-configuration status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: ProviderType,
    /// Credentials are configured; this does not verify validity or entitlement.
    pub authed: bool,
}

/// Set or remove one provider credential. This request contains a secret and
/// intentionally does not implement Debug. `None` removes the credential.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetProviderCredential {
    /// Provider whose credential is being changed.
    pub provider: ProviderType,
    /// New credential, or `None` to remove it. Never returned by the server.
    #[serde(deserialize_with = "deserialize_credential")]
    pub credential: Option<String>,
}

fn deserialize_credential<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// Catalog reference prices in USD per million tokens, not subscription charges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCostRates {
    /// Input token rate; `None` means unknown and zero means free.
    pub input: Option<f64>,
    /// Output token rate; `None` means unknown and zero means free.
    pub output: Option<f64>,
    /// Cached-input read rate; `None` means unknown and zero means free.
    pub cache_read: Option<f64>,
    /// Cache write rate; `None` means unknown and zero means free.
    pub cache_write: Option<f64>,
}

/// Reference pricing tier for requests above an input-token threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostTier {
    /// Input-token count above which these rates apply.
    pub input_tokens_above: u64,
    /// Tier rates, flattened into the serialized tier object.
    #[serde(flatten)]
    pub rates: ModelCostRates,
}

/// Catalog reference pricing, not guaranteed billing or subscription charges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCost {
    /// Base rates, flattened into the serialized cost object; defaults are unknown.
    #[serde(flatten)]
    pub rates: ModelCostRates,
    /// Additional threshold-dependent rates; defaults to empty.
    pub tiers: Vec<ModelCostTier>,
}

/// Supported user effort and its resolved provider-wire representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningOption {
    /// Effort selectable by clients.
    pub effort: Effort,
    /// None means omit reasoning parameters (for non-reasoning models).
    pub wire_effort: Option<Effort>,
}

/// What kind of model a catalog entry is, which determines how it is called.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelKind {
    /// Generates text and tool calls; usable as an agent's model.
    #[default]
    Chat,
    /// Answers typed questions with probabilities; never an agent's model.
    Decision,
}

/// Catalog metadata with live authentication status and persisted pin state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Identity containing both provider and provider-specific API name.
    pub model: Model,
    /// Whether this is a chat or a decision model; entries stored before the
    /// field existed are chat models.
    #[serde(default)]
    pub kind: ModelKind,
    /// Catalog display name.
    pub name: String,
    /// Supported input modality names from the catalog.
    pub input_modalities: Vec<String>,
    /// Context-window token limit, or `None` when unknown.
    pub context_window: Option<u64>,
    /// Output-token limit, or `None` when unknown.
    pub max_output_tokens: Option<u64>,
    /// Reference token pricing.
    pub cost: ModelCost,
    /// Whether the catalog advertises reasoning support.
    pub reasoning: bool,
    /// Selectable effort values and their provider-wire mappings.
    pub reasoning_options: Vec<ReasoningOption>,
    /// Whether provider credentials are configured, without validity verification.
    pub authed: bool,
    /// Server-wide pin state, retained across refreshes and restarts.
    pub pinned: bool,
    /// Last successful catalog revalidation time, serialized as RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub refreshed_at: OffsetDateTime,
}

/// Model catalog filters combined with AND. `None` leaves a field unfiltered;
/// the default query returns all catalog models.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListModels {
    /// Include only models from this provider, or all providers when `None`.
    pub provider: Option<ProviderType>,
    /// Include models whose provider has configured credentials (`Some(true)`)
    /// or lacks them (`Some(false)`). `None` includes both; credentials are not
    /// checked for validity or model access.
    pub authed: Option<bool>,
    /// Include only pinned (`Some(true)`) or unpinned (`Some(false)`) models.
    /// `None` includes both.
    pub pinned: Option<bool>,
    /// Include only chat (`Some(Chat)`) or decision models. `None` includes both.
    #[serde(default)]
    pub kind: Option<ModelKind>,
}

/// Flat query parameters for lookup by provider and API name, without effort.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLookup {
    /// Provider owning the model.
    pub provider: ProviderType,
    /// Provider-specific API name; passed as a query parameter to support slashes.
    pub model: String,
}

/// Change server-wide persisted pin state for a model identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinModel {
    /// Identity containing provider and API name.
    pub model: Model,
    /// Pin when true; unpin when false. Unpinning is idempotent.
    pub pinned: bool,
}

/// Request catalog revalidation without waiting for the freshness interval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResyncModels {
    /// Refresh only this provider's catalog. `None` refreshes all providers.
    pub provider: Option<ProviderType>,
}
