use crate::agents::{
    publication::Publication,
    session_manager::{Runtime, RuntimeError},
};
use crate::{
    config::ResolvedConfig,
    db::{Database, DbError},
    providers::{AuthError, ProviderRegistry},
};
use std::{
    future::{Future, IntoFuture},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::net::TcpListener;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("server I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

pub async fn start(config: ResolvedConfig) -> Result<(), ServerError> {
    if let Some(parent) = config.database_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let auth_path = config.config_path.with_file_name("auth.toml");
    // Validate existing provider credentials before changing the shared file.
    let providers = ProviderRegistry::load(&auth_path).await?;
    let probe_key = crate::auth::ensure_probe_key(&auth_path).await?;
    let database = Database::open(&config.database_path).await?;
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        database = %config.database_path.display(),
        config = %config.config_path.display(),
        address = %listener.local_addr()?,
        "server ready"
    );

    serve_with_probe_key(listener, database, providers, probe_key, shutdown_signal()).await
}

/// Injectable listener and shutdown future permit isolated startup tests.
pub async fn serve(
    listener: TcpListener,
    database: Database,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    serve_with_providers(listener, database, ProviderRegistry::default(), shutdown).await
}

pub async fn serve_with_providers(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    serve_with_probe_key(
        listener,
        database,
        providers,
        psa_api::probes::Uuid::new_v4().to_string(),
        shutdown,
    )
    .await
}

pub async fn serve_with_probe_key(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probe_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let probes = crate::probes::ProbeRegistry::start(probe_key);
    serve_with_probe_registry(listener, database, providers, probes, shutdown).await
}

/// The registry exposes remote-operation handles to server-side integrations.
pub async fn serve_with_probe_registry(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probes: crate::probes::ProbeRegistry,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let publication = Publication::default();
    let runtime = Runtime::start(
        database.clone(),
        publication.clone(),
        providers,
        probes.clone(),
    )
    .await?;
    let accepting = Arc::new(AtomicBool::new(true));
    let app = crate::http::router(crate::http::AppState {
        database: database.clone(),
        publication: publication.clone(),
        runtime: runtime.handle.clone(),
        accepting: accepting.clone(),
        probes: probes.clone(),
    });
    // signals http server to stop when anything sent on stopped
    let (stopped, received) = tokio::sync::oneshot::channel();

    let mut http = Box::pin(
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = received.await;
            })
            .into_future(),
    );

    // wait for either stop signal (app getting killed), or http server error to decide to shutdown
    let early_result = tokio::select! {
        result = &mut http => Some(result),
        _ = shutdown => None,
    };
    accepting.store(false, Ordering::Release);
    // Bundles may own remote resources; keep probes connected while workers and
    // retained bundle owners finish their bounded cleanup.
    let cleanup = runtime.shutdown().await;
    probes.shutdown().await;
    publication.close();

    // shutdown http server if it hasn't yet
    let _ = stopped.send(());
    let result = match early_result {
        Some(result) => result,
        None => http.await,
    };

    database.close().await;
    result?;
    cleanup?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result {
                            tracing::error!(%error, "Ctrl-C handler failed");
                        }
                    }
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::error!(%error, "SIGTERM handler failed");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "Ctrl-C handler failed");
    }
}
