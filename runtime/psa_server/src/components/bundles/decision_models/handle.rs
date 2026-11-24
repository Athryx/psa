//! Session-owned access to decision-model providers: availability and a
//! concurrency cap shared by every agent in the session.

use crate::components::cancellation::Cancellation;

use llm_api::{
    api::decisions::{DecisionRequest, DecisionResponse},
    providers::Model,
};
use psa::api::{ListModels, ModelInfo, ModelKind};
use psa_config::server::DEFAULT_MAX_CONCURRENT_DECISIONS;
use psa_models::{ModelCatalog, ProviderRegistry};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Why a decision call did not produce a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionFailure {
    pub message: String,
    pub cancelled: bool,
}

impl DecisionFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: "Execution cancelled".into(),
            cancelled: true,
        }
    }
}

struct Inner {
    providers: Option<ProviderRegistry>,
    catalog: Option<ModelCatalog>,
    slots: Semaphore,
}

#[derive(Clone)]
pub(crate) struct DecisionHandle {
    inner: Arc<Inner>,
}

impl DecisionHandle {
    /// Without providers every call fails with a configuration error.
    pub(crate) fn new(providers: Option<ProviderRegistry>, catalog: Option<ModelCatalog>) -> Self {
        let limit = providers.as_ref().map_or(
            DEFAULT_MAX_CONCURRENT_DECISIONS,
            ProviderRegistry::max_concurrent_decisions,
        );
        Self {
            inner: Arc::new(Inner {
                providers,
                catalog,
                slots: Semaphore::new(limit.get()),
            }),
        }
    }

    /// Whether credentials for the model's provider are configured.
    pub(crate) fn is_available(&self, model: &Model) -> bool {
        self.inner
            .providers
            .as_ref()
            .is_some_and(|providers| providers.decisions(model.provider).is_some())
    }

    /// Catalog metadata for a decision model. `Ok(None)` means there is no
    /// catalog to consult; a chat or unknown model is an error.
    pub(crate) async fn describe(&self, model: &Model) -> Result<Option<ModelInfo>, String> {
        let Some(catalog) = &self.inner.catalog else {
            return Ok(None);
        };
        let info = catalog
            .model_info(model)
            .await
            .map_err(|error| error.to_string())?;
        match info {
            Some(info) if info.kind == ModelKind::Decision => Ok(Some(info)),
            Some(_) => Err(format!("{} is not a decision model", model.api_name)),
            None => Err(format!("{} is not in the model catalog", model.api_name)),
        }
    }

    /// Pinned decision models in catalog order; none without a catalog.
    pub(crate) async fn pinned_models(&self) -> Result<Vec<Model>, String> {
        let Some(catalog) = &self.inner.catalog else {
            return Ok(Vec::new());
        };
        let query = ListModels {
            pinned: Some(true),
            kind: Some(ModelKind::Decision),
            ..Default::default()
        };
        let models = catalog
            .list_models(query)
            .await
            .map_err(|error| error.to_string())?;
        Ok(models.into_iter().map(|info| info.model).collect())
    }

    /// Send one request, waiting for a free slot first.
    pub(crate) async fn call(
        &self,
        request: DecisionRequest,
        cancellation: &mut Cancellation,
    ) -> Result<DecisionResponse, DecisionFailure> {
        let client = self
            .inner
            .providers
            .as_ref()
            .and_then(|providers| providers.decisions(request.model.provider))
            .ok_or_else(|| {
                DecisionFailure::new(format!(
                    "No credentials for {}; decision models are unavailable",
                    request.model.provider.name()
                ))
            })?;

        let _slot = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(DecisionFailure::cancelled()),
            slot = self.inner.slots.acquire() => slot.map_err(|_| DecisionFailure::new("Decision calls are shut down"))?,
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(DecisionFailure::cancelled()),
            response = client.decide(&request) => {
                response.map_err(|error| DecisionFailure::new(error.to_string()))
            }
        }
    }
}
