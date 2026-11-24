//! Cooperative cancellation for a single invocation.

use tokio::sync::oneshot;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_latched_and_wait_is_cancel_safe() {
        let (sender, mut cancellation) = Cancellation::channel();
        assert!(!cancellation.is_cancelled());
        tokio::select! { biased; _ = std::future::ready(()) => {}, _ = cancellation.cancelled() => panic!() }
        sender.send(()).unwrap();
        cancellation.cancelled().await;
        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
        let (sender, mut cancellation) = Cancellation::channel();
        drop(sender);
        cancellation.cancelled().await;
        assert!(cancellation.is_cancelled());
    }
}
