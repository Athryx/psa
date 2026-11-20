use clap::{Parser, Subcommand};
use psa_probe::image_hooks::ImageHooks;
#[derive(Parser)]
#[command(about = "PSA remote execution probe", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Start(psa_probe::config::StartOptions),
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let Command::Start(options) = Cli::parse().command;
    let hooks = options.image_hooks.then(ImageHooks::from_env);
    let config = options.resolve().await?;
    let data_dir = config.metadata.data_dir.clone();

    // Register for SIGTERM before setup so a stop during setup still tears down.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    let shutdown = async move {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    };
    tokio::pin!(shutdown);

    let Some(hooks) = hooks else {
        psa_probe::run(config, shutdown).await?;
        return Ok(());
    };

    let stopped = tokio::select! {
        _ = hooks.run_setup(&data_dir) => false,
        _ = &mut shutdown => true,
    };
    let result = if stopped {
        Ok(())
    } else {
        psa_probe::run(config, shutdown).await
    };
    hooks.run_teardown(&data_dir).await;
    result?;

    Ok(())
}
