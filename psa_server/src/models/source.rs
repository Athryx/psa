//! Pi catalog transport and normalization; upstream URLs and headers are never imported.
use super::{CatalogError, ProviderSyncState};
use llm_api::providers::{Effort, Model, ProviderType};
use psa_api::{ModelCost, ModelCostRates, ModelCostTier, ModelInfo, ReasoningOption};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, time::Duration};
use time::OffsetDateTime;

#[derive(Clone)]
pub struct CatalogSource {
    base_url: reqwest::Url,
    client: reqwest::Client,
}

pub(crate) enum CatalogUpdate {
    Unchanged,
    Models {
        models: Vec<ModelInfo>,
        etag: Option<String>,
    },
}

impl CatalogSource {
    pub fn new(base_url: &str) -> Result<Self, CatalogError> {
        let base_url = reqwest::Url::parse(base_url)
            .map_err(|_| CatalogError::Invalid("invalid catalog base URL".into()))?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.cannot_be_a_base()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(CatalogError::Invalid(
                "catalog base URL must be HTTP(S) without credentials, query, or fragment".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!(
                "pi/1.1.0 (compatible; psa/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()
            .map_err(CatalogError::Http)?;
        Ok(Self { base_url, client })
    }

    pub fn pi() -> Self {
        Self::new("https://pi.dev").expect("static catalog URL and TLS client are valid")
    }

    pub(crate) async fn fetch(
        &self,
        provider: ProviderType,
        state: &ProviderSyncState,
    ) -> Result<CatalogUpdate, CatalogError> {
        let id = match provider {
            ProviderType::OpenAiCodex => "openai-codex",
            ProviderType::Openrouter => "openrouter",
        };
        let mut url = self.base_url.clone();
        url.set_path(&format!("/api/models/providers/{id}"));
        url.set_query(Some("types=chat"));
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(etag) = &state.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await?.error_for_status()?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if state.last_success.is_none() {
                return Err(CatalogError::Invalid(
                    "catalog returned 304 without a cached catalog".into(),
                ));
            }
            return Ok(CatalogUpdate::Unchanged);
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > 32 * 1024 * 1024 {
                return Err(CatalogError::Invalid("catalog exceeds 32 MiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        let models = normalize_catalog(provider, serde_json::from_slice(&body)?)?;
        Ok(CatalogUpdate::Models { models, etag })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiModel {
    id: String,
    name: Option<String>,
    #[serde(default)]
    input: Vec<String>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    cost: Option<PiCost>,
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: HashMap<String, Option<String>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiRates {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl PiRates {
    fn into_rates(self) -> Result<ModelCostRates, CatalogError> {
        if [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || value < 0.0)
        {
            return Err(CatalogError::Invalid("invalid catalog price".into()));
        }
        Ok(ModelCostRates {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        })
    }
}

#[derive(Deserialize)]
struct PiCost {
    #[serde(flatten)]
    rates: PiRates,
    #[serde(default)]
    tiers: Vec<PiTier>,
}

impl PiCost {
    fn into_cost(self) -> Result<ModelCost, CatalogError> {
        let rates = self.rates.into_rates()?;
        let tiers = self
            .tiers
            .into_iter()
            .map(|tier| {
                Ok(ModelCostTier {
                    input_tokens_above: tier.input_tokens_above,
                    rates: tier.rates.into_rates()?,
                })
            })
            .collect::<Result<_, CatalogError>>()?;
        Ok(ModelCost { rates, tiers })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiTier {
    input_tokens_above: u64,
    #[serde(flatten)]
    rates: PiRates,
}

fn normalize_catalog(provider: ProviderType, value: Value) -> Result<Vec<ModelInfo>, CatalogError> {
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(mut object) => match object.remove("models") {
            Some(Value::Array(entries)) => entries,
            Some(_) => return Err(CatalogError::Invalid("invalid catalog models array".into())),
            None => object.into_values().collect(),
        },
        _ => return Err(CatalogError::Invalid("invalid catalog response".into())),
    };
    let mut models = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for value in entries {
        if value.get("type").is_some_and(|kind| kind != "chat") {
            continue;
        }
        let raw: PiModel = serde_json::from_value(value)?;
        let model =
            Model::new(raw.id.clone()).map_err(|error| CatalogError::Invalid(error.to_string()))?;
        if !ids.insert(raw.id.clone()) {
            return Err(CatalogError::Invalid("duplicate catalog model ID".into()));
        }
        let reasoning_options = normalize_reasoning(raw.reasoning, &raw.thinking_level_map)?;
        let cost = raw
            .cost
            .map(PiCost::into_cost)
            .transpose()?
            .unwrap_or_default();

        models.push(ModelInfo {
            provider,
            model,
            name: raw
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(raw.id),
            input_modalities: raw.input,
            context_window: raw.context_window.filter(|value| *value > 0),
            max_output_tokens: raw.max_tokens.filter(|value| *value > 0),
            cost,
            reasoning: raw.reasoning,
            reasoning_options,
            authed: false,
            pinned: false,
            refreshed_at: OffsetDateTime::now_utc(),
        });
    }
    Ok(models)
}

fn normalize_reasoning(
    reasoning: bool,
    mappings: &HashMap<String, Option<String>>,
) -> Result<Vec<ReasoningOption>, CatalogError> {
    if !reasoning {
        return Ok(vec![ReasoningOption {
            effort: Effort::Off,
            wire_effort: None,
        }]);
    }
    let mut options = Vec::new();
    for effort in [
        Effort::Off,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ] {
        let mapped = match mappings.get(effort.as_str()) {
            Some(None) => continue,
            None if matches!(effort, Effort::Xhigh | Effort::Max) => continue,
            None => effort,
            Some(Some(value)) => match value.as_str() {
                "off" | "none" => Effort::Off,
                "minimal" => Effort::Minimal,
                "low" => Effort::Low,
                "medium" => Effort::Medium,
                "high" => Effort::High,
                "xhigh" => Effort::Xhigh,
                "max" => Effort::Max,
                _ => {
                    return Err(CatalogError::Invalid(format!(
                        "unknown reasoning effort mapping: {value}"
                    )));
                }
            },
        };
        options.push(ReasoningOption {
            effort,
            wire_effort: Some(mapped),
        });
    }
    Ok(options)
}

#[cfg(test)]
mod tests;
