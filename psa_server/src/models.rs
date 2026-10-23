//! Persistent model discovery. The actor owns refresh scheduling, never inference.
mod source;

use crate::{
    db::{Database, DbError},
    providers::ProviderRegistry,
};
use actors::{Reply, UnboundedMailbox, UnboundedMailboxHandle};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use llm_api::{
    api::ReasoningOverride,
    providers::{Model, ModelConfig, ProviderType},
};
use psa_api::{ListModels, ModelInfo, ModelSyncResult, PinModel, ProviderInfo};
use serde::{Deserialize, Serialize};
pub use source::CatalogSource;
use source::CatalogUpdate;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{task::JoinHandle, time::Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("catalog request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("model is not in the catalog")]
    NotFound,
    #[error("model catalog is shutting down or unavailable")]
    Unavailable,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct ProviderSyncState {
    pub etag: Option<String>,
    pub checked_at: Option<i64>,
    pub last_success: Option<i64>,
}

#[derive(Clone)]
pub struct ModelCatalog {
    database: Database,
    authed: Vec<ProviderType>,
    commands: UnboundedMailboxHandle<Command>,
}

impl ModelCatalog {
    pub fn providers(&self) -> Vec<ProviderInfo> {
        ProviderType::ALL
            .into_iter()
            .map(|id| ProviderInfo {
                id,
                name: match id {
                    ProviderType::OpenAiCodex => "OpenAI Codex",
                    ProviderType::Openrouter => "OpenRouter",
                }
                .into(),
                authed: self.authed.contains(&id),
            })
            .collect()
    }

    pub async fn list_models(&self, query: ListModels) -> Result<Vec<ModelInfo>, CatalogError> {
        let mut cursor = self.database.cursor().await?;
        let mut models = cursor.list_model_info().await?;
        cursor.rollback().await?;

        for model in &mut models {
            model.authed = self.authed.contains(&model.provider);
        }
        models.retain(|model| {
            query
                .provider
                .is_none_or(|provider| model.provider == provider)
                && query.authed.is_none_or(|authed| model.authed == authed)
                && query.pinned.is_none_or(|pinned| model.pinned == pinned)
        });
        Ok(models)
    }

    pub async fn model_info(
        &self,
        config: &ModelConfig,
    ) -> Result<Option<ModelInfo>, CatalogError> {
        self.lookup(config.provider, &config.model).await
    }

    pub async fn lookup(
        &self,
        provider: ProviderType,
        model: &Model,
    ) -> Result<Option<ModelInfo>, CatalogError> {
        let mut cursor = self.database.cursor().await?;
        let mut info = cursor.get_model_info(provider, model).await?;
        cursor.rollback().await?;

        if let Some(info) = &mut info {
            info.authed = self.authed.contains(&provider);
        }
        Ok(info)
    }

    pub async fn set_pinned(&self, request: PinModel) -> Result<Option<ModelInfo>, CatalogError> {
        Model::new(request.model.api_name.clone())
            .map_err(|error| CatalogError::Invalid(error.to_string()))?;
        let mut cursor = self.database.cursor().await?;
        let mut info = cursor
            .get_model_info(request.provider, &request.model)
            .await?;
        if request.pinned && info.is_none() {
            return Err(CatalogError::NotFound);
        }
        cursor
            .set_model_pinned(request.provider, &request.model, request.pinned)
            .await?;
        cursor.commit().await?;

        if let Some(info) = &mut info {
            info.pinned = request.pinned;
            info.authed = self.authed.contains(&request.provider);
        }
        Ok(info)
    }

    pub async fn resync(
        &self,
        provider: Option<ProviderType>,
    ) -> Result<Vec<ModelSyncResult>, CatalogError> {
        self.commands
            .request(|reply| Command::Resync { provider, reply })
            .await
            .map_err(|_| CatalogError::Unavailable)
    }

    pub async fn resolve_reasoning(
        &self,
        config: &ModelConfig,
    ) -> Result<Option<ReasoningOverride>, CatalogError> {
        let Some(info) = self.model_info(config).await? else {
            return Ok(None);
        };
        let Some(option) = info
            .reasoning_options
            .iter()
            .find(|option| option.effort == config.effort)
        else {
            let supported: Vec<_> = info
                .reasoning_options
                .iter()
                .map(|option| option.effort.as_str())
                .collect();
            return Err(CatalogError::Invalid(format!(
                "Reasoning effort {} is unsupported for {}. Supported: {}",
                config.effort.as_str(),
                info.model.api_name,
                supported.join(", ")
            )));
        };
        Ok(Some(match option.wire_effort {
            Some(effort) => ReasoningOverride::Effort(effort),
            None => ReasoningOverride::Omit,
        }))
    }
}

struct AbortTask(JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct ModelCatalogRuntime {
    pub handle: ModelCatalog,
    task: Option<AbortTask>,
}

impl ModelCatalogRuntime {
    pub async fn start(
        database: Database,
        providers: &ProviderRegistry,
        source: CatalogSource,
    ) -> Result<Self, CatalogError> {
        let mut cursor = database.cursor().await?;
        let mut deadlines = HashMap::new();
        for provider in ProviderType::ALL {
            let stored = cursor.load_provider_sync(provider).await?;
            let age = stored
                .checked_at
                .filter(|_| stored.last_success.is_some())
                .map(|checked| {
                    OffsetDateTime::now_utc()
                        .unix_timestamp()
                        .saturating_sub(checked)
                        .max(0) as u64
                });
            let remaining = age.map_or(Duration::ZERO, |age| {
                REFRESH_INTERVAL.saturating_sub(Duration::from_secs(age))
            });
            deadlines.insert(provider, Instant::now() + remaining);
        }
        cursor.rollback().await?;

        let (commands, mailbox) = actors::unbounded_channel();
        let handle = ModelCatalog {
            database: database.clone(),
            authed: ProviderType::ALL
                .into_iter()
                .filter(|provider| providers.provider(*provider).is_some())
                .collect(),
            commands,
        };
        let state = State {
            database,
            source,
            deadlines,
            active: HashSet::new(),
            jobs: FuturesUnordered::new(),
            waiters: Vec::new(),
        };
        let task = tokio::spawn(state.run(mailbox));
        Ok(Self {
            handle,
            task: Some(AbortTask(task)),
        })
    }

    pub async fn shutdown(&mut self) -> Result<(), CatalogError> {
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        let _ = self.handle.commands.send(Command::Shutdown);
        tokio::time::timeout(Duration::from_secs(2), &mut task.0)
            .await
            .map_err(|_| CatalogError::Unavailable)?
            .map_err(|_| CatalogError::Unavailable)?;
        Ok(())
    }
}

enum Command {
    Resync {
        provider: Option<ProviderType>,
        reply: Reply<Vec<ModelSyncResult>>,
    },
    Shutdown,
}

struct RefreshWaiter {
    remaining: HashSet<ProviderType>,
    results: Vec<ModelSyncResult>,
    reply: Reply<Vec<ModelSyncResult>>,
}

type RefreshJob = BoxFuture<'static, (ProviderType, Result<(), CatalogError>)>;

struct State {
    database: Database,
    source: CatalogSource,
    deadlines: HashMap<ProviderType, Instant>,
    active: HashSet<ProviderType>,
    jobs: FuturesUnordered<RefreshJob>,
    waiters: Vec<RefreshWaiter>,
}

impl State {
    fn start_refresh(&mut self, provider: ProviderType) {
        if !self.active.insert(provider) {
            return;
        }
        let database = self.database.clone();
        let source = self.source.clone();
        self.jobs.push(Box::pin(async move {
            (provider, sync_provider(&database, &source, provider).await)
        }));
    }

    fn register_refresh(
        &mut self,
        provider: Option<ProviderType>,
        reply: Reply<Vec<ModelSyncResult>>,
    ) {
        let providers: HashSet<_> = provider.map_or_else(
            || ProviderType::ALL.into_iter().collect(),
            |provider| HashSet::from([provider]),
        );
        for provider in &providers {
            self.start_refresh(*provider);
        }
        self.waiters.push(RefreshWaiter {
            remaining: providers,
            results: Vec::new(),
            reply,
        });
    }

    fn handle_refresh_result(&mut self, provider: ProviderType, result: Result<(), CatalogError>) {
        self.active.remove(&provider);
        self.deadlines
            .insert(provider, Instant::now() + REFRESH_INTERVAL);
        let result = ModelSyncResult {
            provider,
            success: result.is_ok(),
            error: result.err().map(|error| error.to_string()),
        };
        if let Some(error) = &result.error {
            tracing::warn!(?provider, %error, "model catalog sync failed");
        }

        for waiter in &mut self.waiters {
            if waiter.remaining.remove(&provider) {
                waiter.results.push(result.clone());
            }
        }
        let mut pending = Vec::new();
        for mut waiter in self.waiters.drain(..) {
            if waiter.remaining.is_empty() {
                waiter
                    .results
                    .sort_by_key(|result| result.provider.as_str());
                let _ = waiter.reply.send(waiter.results);
            } else if !waiter.reply.is_closed() {
                pending.push(waiter);
            }
        }
        self.waiters = pending;
    }

    async fn run(mut self, mut mailbox: UnboundedMailbox<Command>) {
        loop {
            let due: Vec<_> = self
                .deadlines
                .iter()
                .filter(|(provider, deadline)| {
                    **deadline <= Instant::now() && !self.active.contains(provider)
                })
                .map(|(provider, _)| *provider)
                .collect();
            for provider in due {
                self.start_refresh(provider);
            }

            let next = self
                .deadlines
                .iter()
                .filter(|(provider, _)| !self.active.contains(provider))
                .map(|(_, deadline)| *deadline)
                .min()
                .unwrap_or_else(|| Instant::now() + REFRESH_INTERVAL);
            tokio::select! {
                biased;
                command = mailbox.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(Command::Resync { provider, reply }) => self.register_refresh(provider, reply),
                },
                Some((provider, result)) = self.jobs.next(), if !self.jobs.is_empty() => {
                    self.handle_refresh_result(provider, result);
                },
                _ = tokio::time::sleep_until(next) => {},
            }
        }
        // Jobs and outstanding replies are dropped; never await HTTP during shutdown.
    }
}

async fn sync_provider(
    database: &Database,
    source: &CatalogSource,
    provider: ProviderType,
) -> Result<(), CatalogError> {
    let mut cursor = database.cursor().await?;
    let mut state = cursor.load_provider_sync(provider).await?;
    cursor.rollback().await?;

    let update = source.fetch(provider, &state).await;
    state.checked_at = Some(OffsetDateTime::now_utc().unix_timestamp());
    let mut cursor = database.cursor().await?;
    match &update {
        Ok(CatalogUpdate::Models { models, etag }) => {
            cursor.replace_model_catalog(provider, models).await?;
            state.etag = etag.clone();
            state.last_success = state.checked_at;
        }
        Ok(CatalogUpdate::Unchanged) => {
            state.last_success = state.checked_at;
        }
        Err(_) => {}
    }
    cursor.save_provider_sync(provider, &state).await?;
    cursor.commit().await?;
    update.map(|_| ())
}

#[cfg(test)]
mod tests;
