use crate::agents::{
    publication::Publication,
    session_manager::{Runtime, RuntimeError},
};
use crate::{
    config::ResolvedConfig,
    db::{Database, DbError},
    providers::{AuthError, ProviderRegistry},
};
use llm_api::providers::ModelConfig;
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
    #[error(transparent)]
    Catalog(#[from] crate::models::CatalogError),
    #[error(transparent)]
    ProbeConfig(#[from] psa_probe::config::ConfigError),
}

pub async fn start(config: ResolvedConfig) -> Result<(), ServerError> {
    start_with_shutdown(config, shutdown_signal()).await
}

/// Start a configured server with an injectable shutdown signal.
pub async fn start_with_shutdown(
    config: ResolvedConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    if let Some(parent) = config.database_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    #[cfg(unix)]
    let local_lock = crate::local_control::LocalControl::acquire(&config.data_dir)?;

    let auth_path = config.config_path.with_file_name("auth.toml");
    // Validate existing provider credentials before changing the shared file.
    let providers = ProviderRegistry::load(&auth_path)
        .await?
        .with_auth_path(auth_path.clone())
        .with_max_concurrent_decisions(config.max_concurrent_decision_calls);
    let keys = crate::auth::ensure_keys(&auth_path).await?;
    let database = Database::open(&config.database_path).await?;
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        database = %config.database_path.display(),
        config = %config.config_path.display(),
        address = %listener.local_addr()?,
        "server ready"
    );

    let local_probe = if config.autostart_local_probe {
        let mut address = listener.local_addr()?;
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        let probe_config = psa_probe::config::StartOptions {
            data_dir: Some(config.data_dir.clone()),
            server: Some(format!("http://{address}")),
            ..Default::default()
        }
        .resolve_with_server_config(Some(&config.config_path))
        .await?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(error) = psa_probe::run(probe_config, async {
                let _ = stopped.await;
            })
            .await
            {
                tracing::error!(%error, "local probe stopped");
            }
        });
        Some((stop, task))
    } else {
        None
    };
    #[cfg(unix)]
    let (_local_control, local_stop) = {
        let mut address = listener.local_addr()?;
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        crate::local_control::LocalControl::start(
            local_lock,
            &config.data_dir,
            address,
            &keys.client_key,
        )?
    };
    #[cfg(unix)]
    let shutdown = async move {
        tokio::select! { _ = shutdown => (), _ = local_stop => () }
    };

    let probes = crate::probes::ProbeRegistry::start(keys.probe_key);
    let result = serve_with_client_config(
        listener,
        database,
        providers,
        probes,
        crate::config_registry::ConfigRegistry::new(config.data_dir)
            .with_agent_defaults(config.agent_defaults),
        config.session_defaults,
        psa_api::ClientConfig {
            default_agent_name: config.default_agent_name,
            default_sandbox_image: config.default_sandbox_image,
        },
        keys.client_key,
        shutdown,
    )
    .await;
    if let Some((stop, task)) = local_probe {
        let _ = stop.send(());
        if let Err(error) = task.await {
            tracing::error!(%error, "local probe task failed");
        }
    }
    result
}

/// Injectable listener and shutdown future permit isolated startup tests.
pub async fn serve(
    listener: TcpListener,
    database: Database,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    serve_with_providers(
        listener,
        database,
        ProviderRegistry::default(),
        client_key,
        shutdown,
    )
    .await
}

pub async fn serve_with_providers(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    serve_with_probe_key(
        listener,
        database,
        providers,
        psa_api::probes::Uuid::new_v4().to_string(),
        client_key,
        shutdown,
    )
    .await
}

pub async fn serve_with_probe_key(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probe_key: String,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let probes = crate::probes::ProbeRegistry::start(probe_key);
    serve_with_probe_registry(listener, database, providers, probes, client_key, shutdown).await
}

/// The registry exposes remote-operation handles to server-side integrations.
pub async fn serve_with_probe_registry(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probes: crate::probes::ProbeRegistry,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let data_dir = directories::ProjectDirs::from("", "", "psa")
        .ok_or_else(|| std::io::Error::other("cannot determine PSA data directory"))?
        .data_local_dir()
        .to_path_buf();
    serve_with_configuration(
        listener,
        database,
        providers,
        probes,
        crate::config_registry::ConfigRegistry::new(data_dir),
        crate::config::default_model_config(),
        client_key,
        shutdown,
    )
    .await
}

/// Serve with explicit file-discovery/upload roots and new-session model defaults.
/// `default_model_config` is used only when a creation request omits its model;
/// persisted sessions and forked session models are not reset to these defaults.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_configuration(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probes: crate::probes::ProbeRegistry,
    configuration: crate::config_registry::ConfigRegistry,
    default_model_config: ModelConfig,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    serve_with_client_config(
        listener,
        database,
        providers,
        probes,
        configuration,
        default_model_config,
        psa_api::ClientConfig {
            default_agent_name: "root".into(),
            default_sandbox_image: None,
        },
        client_key,
        shutdown,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_with_client_config(
    listener: TcpListener,
    database: Database,
    providers: ProviderRegistry,
    probes: crate::probes::ProbeRegistry,
    configuration: crate::config_registry::ConfigRegistry,
    default_model_config: ModelConfig,
    client_config: psa_api::ClientConfig,
    client_key: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServerError> {
    let mut catalog = crate::models::ModelCatalogRuntime::start(
        database.clone(),
        &providers,
        providers.catalog_source(),
    )
    .await?;
    let providers = providers.with_catalog(catalog.handle.clone());
    let publication = Publication::default();
    let runtime = Runtime::start_with_configuration(
        database.clone(),
        publication.clone(),
        providers,
        probes.clone(),
        Some(configuration),
        default_model_config,
    )
    .await?;
    let accepting = Arc::new(AtomicBool::new(true));
    let app = crate::http::router(crate::http::AppState {
        database: database.clone(),
        publication: publication.clone(),
        runtime: runtime.handle.clone(),
        accepting: accepting.clone(),
        client_key: client_key.into(),
        client_config,
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
    let catalog_cleanup = catalog.shutdown().await;
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
    catalog_cleanup?;
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
