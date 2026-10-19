use clap::Parser;
use psa_server::config::{Cli, Command};

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error(transparent)]
    Config(#[from] psa_server::config::ConfigError),
    #[error(transparent)]
    Server(#[from] psa_server::server::ServerError),
}

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "psa_server=info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Start(options) => psa_server::server::start(options.resolve().await?).await?,
    }
    Ok(())
}
