use crate::agents::agent_manager::AgentManagerMessage;
use actors::UnboundedMailbox;
pub use psa_shared::cancellation::Cancellation;
use psa_store::Session;
use tokio::sync::oneshot;

pub(crate) async fn wait_for_interrupt(
    session: &mut Session,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) {
    while let Some(message) = controls.recv().await {
        match message {
            AgentManagerMessage::Interrupt => return,
            AgentManagerMessage::SessionUpdated { session: updated } => *session = updated,
        }
    }
}

pub(crate) async fn run_cancellable<F: std::future::Future>(
    future: F,
    sender: oneshot::Sender<()>,
    grace: std::time::Duration,
    session: &mut Session,
    controls: &mut UnboundedMailbox<AgentManagerMessage>,
) -> (Option<F::Output>, bool) {
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = wait_for_interrupt(session, controls) => {
            let _ = sender.send(());
            (tokio::time::timeout(grace, &mut future).await.ok(), true)
        }
        result = &mut future => (Some(result), false),
    }
}

#[cfg(test)]
#[path = "../tests/components/cancellation/tests.rs"]
mod tests;
