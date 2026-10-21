use clap::{Parser, Subcommand};
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
    let config = options.resolve().await?;
    psa_probe::run(config, async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    })
    .await?;
    Ok(())
}
