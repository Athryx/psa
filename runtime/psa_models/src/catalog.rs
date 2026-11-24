//! Persistent model discovery. The actor owns refresh scheduling, never inference.
mod source;

use crate::providers::ProviderRegistry;

use actors::{Reply, UnboundedMailbox, UnboundedMailboxHandle};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use llm_api::{
    api::ReasoningOverride,
    providers::{Model, ModelConfig, ProviderType},
};
use psa::api::{ListModels, ModelInfo, PinModel, ProviderInfo};
use psa_store::{Database, DbError};
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
    #[error("model catalog sync failed: {0}")]
    SyncFailed(String),
}

#[derive(Clone)]
pub struct ModelCatalog {
    database: Database,
    providers: ProviderRegistry,
    commands: UnboundedMailboxHandle<Command>,
}

impl ModelCatalog {
    pub fn providers(&self) -> Vec<ProviderInfo> {
        ProviderType::ALL
            .into_iter()
            .map(|id| ProviderInfo {
                id,
                authed: self.providers.provider(id).is_some(),
            })
            .collect()
    }

    pub async fn set_provider_credential(
        &self,
        request: psa::api::SetProviderCredential,
    ) -> Result<ProviderInfo, crate::providers::AuthError> {
        // A disconnected client must not cancel between the durable rename and
        // publishing the new in-memory snapshot.
        let providers = self.providers.clone();
        tokio::spawn(async move { providers.set_credential(request).await })
            .await
            .map_err(|_| crate::providers::AuthError::Unavailable)?
    }

    pub async fn list_models(&self, query: ListModels) -> Result<Vec<ModelInfo>, CatalogError> {
        let mut cursor = self.database.read().await?;
        let mut models = cursor.list_model_info().await?;
        cursor.close().await?;

        for model in &mut models {
            model.authed = self.providers.provider(model.model.provider).is_some();
        }
        models.retain(|model| {
            query
                .provider
                .is_none_or(|provider| model.model.provider == provider)
                && query.authed.is_none_or(|authed| model.authed == authed)
                && query.pinned.is_none_or(|pinned| model.pinned == pinned)
                && query.kind.is_none_or(|kind| model.kind == kind)
        });
        Ok(models)
    }

    pub async fn model_info(&self, model: &Model) -> Result<Option<ModelInfo>, CatalogError> {
        let mut cursor = self.database.read().await?;
        let mut info = cursor.get_model_info(model).await?;
        cursor.close().await?;

        if let Some(info) = &mut info {
            info.authed = self.providers.provider(model.provider).is_some();
        }
        Ok(info)
    }

    pub async fn set_pinned(&self, request: PinModel) -> Result<Option<ModelInfo>, CatalogError> {
        Model::new(request.model.provider, request.model.api_name.clone())
            .map_err(|error| CatalogError::Invalid(error.to_string()))?;
        let mut cursor = self.database.write().await?;
        let mut info = cursor.get_model_info(&request.model).await?;
        if request.pinned && info.is_none() {
            return Err(CatalogError::NotFound);
        }
        cursor
            .set_model_pinned(&request.model, request.pinned)
            .await?;
        cursor.commit().await?;

        if let Some(info) = &mut info {
            info.pinned = request.pinned;
            info.authed = self.providers.provider(request.model.provider).is_some();
        }
        Ok(info)
    }

    pub async fn resync(&self, provider: Option<ProviderType>) -> Result<(), CatalogError> {
        self.commands
            .request(|reply| Command::Resync { provider, reply })
            .await
            .map_err(|_| CatalogError::Unavailable)?
    }

    pub async fn resolve_reasoning(
        &self,
        config: &ModelConfig,
    ) -> Result<Option<ReasoningOverride>, CatalogError> {
        let Some(info) = self.model_info(&config.model).await? else {
            return Ok(None);
        };
        if info.kind != psa::api::ModelKind::Chat {
            return Err(CatalogError::Invalid(format!(
                "{} is a decision model and cannot generate chat responses",
                info.model.api_name
            )));
        }
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
        let mut cursor = database.read().await?;
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
        cursor.close().await?;

        let (commands, mailbox) = actors::unbounded_channel();
        let handle = ModelCatalog {
            database: database.clone(),
            providers: providers.clone(),
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
        reply: Reply<Result<(), CatalogError>>,
    },
    Shutdown,
}

struct RefreshWaiter {
    remaining: HashSet<ProviderType>,
    failures: Vec<String>,
    reply: Reply<Result<(), CatalogError>>,
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
        reply: Reply<Result<(), CatalogError>>,
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
            failures: Vec::new(),
            reply,
        });
    }

    fn handle_refresh_result(&mut self, provider: ProviderType, result: Result<(), CatalogError>) {
        self.active.remove(&provider);
        self.deadlines
            .insert(provider, Instant::now() + REFRESH_INTERVAL);
        let error = result
            .err()
            .map(|error| format!("{}: {error}", provider.name()));
        if let Some(error) = &error {
            tracing::warn!(?provider, %error, "model catalog sync failed");
        }

        for waiter in &mut self.waiters {
            if waiter.remaining.remove(&provider)
                && let Some(error) = &error
            {
                waiter.failures.push(error.clone());
            }
        }
        let mut pending = Vec::new();
        for mut waiter in self.waiters.drain(..) {
            if waiter.remaining.is_empty() {
                waiter.failures.sort();
                let result = if waiter.failures.is_empty() {
                    Ok(())
                } else {
                    Err(CatalogError::SyncFailed(waiter.failures.join("; ")))
                };
                let _ = waiter.reply.send(result);
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
    let mut cursor = database.read().await?;
    let mut state = cursor.load_provider_sync(provider).await?;
    cursor.close().await?;

    let update = source.fetch(provider, &state).await;
    state.checked_at = Some(OffsetDateTime::now_utc().unix_timestamp());
    let mut cursor = database.write().await?;
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
