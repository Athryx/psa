//! Run one message through an agent and print its result and transcript.
//!
//! ```sh
//! PSA_URL=http://localhost:8080 PSA_CLIENT_KEY=... \
//!     cargo run -p psa --example run_agent -- /path/on/host "Summarize README.md"
//! ```
use futures_util::StreamExt;
use psa::{AgentLayout, AgentSpec, Context, EventRange, EventUpdate, SessionSpec};

#[tokio::main]
async fn main() -> psa::Result<()> {
    let url = std::env::var("PSA_URL").unwrap_or_else(|_| "http://localhost:8080".into());
    let key = std::env::var("PSA_CLIENT_KEY").unwrap_or_default();
    let mut args = std::env::args().skip(1);
    let directory = args.next().unwrap_or_else(|| ".".into());
    let message = args.next().unwrap_or_else(|| "Say hello.".into());

    let context = Context::connect(&url, &key, "psa-example")?
        .working_directory(directory)
        .build()
        .await?;
    let session = context.ensure_session(SessionSpec::new("example")).await?;
    let agent = session
        .ensure_agent(AgentSpec::new(
            "assistant",
            AgentLayout::new("assistant").description("Example agent"),
        ))
        .await?;

    // Print events as they commit while the run is in progress.
    let mut feed = agent.stream(EventRange::follow()).await?;
    let printer = tokio::spawn(async move {
        while let Some(Ok(update)) = feed.next().await {
            if let EventUpdate::Committed(event) = update {
                println!("event {}: {:?}", event.event.id.0, event.event.kind);
            }
        }
    });

    let outcome = agent.run(message).await?;
    printer.abort();
    println!("finished {:?}", outcome.state);
    if let Some(result) = outcome.result {
        println!("result: {:?}", result.value);
    }
    Ok(())
}
