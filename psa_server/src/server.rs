use crate::{
    config::ResolvedConfig,
    db::{Database, DbError},
};
use axum::{Router, routing::get};
use std::future::Future;
use tokio::net::TcpListener;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("server I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn start(config: ResolvedConfig) -> Result<(), ServerError> {
    if let Some(parent) = config.database_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let database = Database::open(&config.database_path).await?;
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        database = %config.database_path.display(),
        config = %config.config_path.display(),
        address = %listener.local_addr()?,
        "server ready"
    );

    serve(listener, database, shutdown_signal()).await
}

/// Injectable listener and shutdown future permit isolated startup tests.
pub async fn serve(
    listener: TcpListener,
    database: Database,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let app = Router::new()
        .route("/health", get(|| async { "ok\n" }))
        .with_state(database.clone());

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;

    database.close().await;
    result?;
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
