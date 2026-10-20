use crate::{agents::agent_manager::AgentManagerMessage, sessions::Session};
use tokio::sync::{mpsc, oneshot};

/// Single-invocation cooperative cancellation. Sender drop cancels as well.
pub struct Cancellation {
    receiver: oneshot::Receiver<()>,
    cancelled: bool,
}

impl Cancellation {
    pub fn channel() -> (oneshot::Sender<()>, Self) {
        let (sender, receiver) = oneshot::channel();
        (
            sender,
            Self {
                receiver,
                cancelled: false,
            },
        )
    }

    pub fn is_cancelled(&mut self) -> bool {
        if !self.cancelled {
            self.cancelled = !matches!(
                self.receiver.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );
        }
        self.cancelled
    }

    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        let _ = (&mut self.receiver).await;
        self.cancelled = true;
    }
}

pub(crate) async fn wait_for_interrupt(
    session: &mut Session,
    controls: &mut mpsc::UnboundedReceiver<AgentManagerMessage>,
) {
    while let Some(message) = controls.recv().await {
        match message {
            AgentManagerMessage::Interrupt => return,
            AgentManagerMessage::SessionUpdated(updated) => *session = updated,
        }
    }
}

pub(crate) async fn run_cancellable<F: std::future::Future>(
    future: F,
    sender: oneshot::Sender<()>,
    grace: std::time::Duration,
    session: &mut Session,
    controls: &mut mpsc::UnboundedReceiver<AgentManagerMessage>,
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
mod tests;
