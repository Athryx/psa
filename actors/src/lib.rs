#![doc = include_str!("../README.md")]

use tokio::sync::{mpsc, oneshot};

pub use actors_macros::mailbox;
pub use tokio::sync::mpsc::error::TryRecvError;

/// A reply sender retained and completed explicitly by the receiving actor.
pub type Reply<T> = oneshot::Sender<T>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MailboxError {
    #[error("mailbox is closed")]
    Closed,
    #[error("reply sender was dropped without a reply")]
    ReplyDropped,
}

/// Receiving half of a bounded mailbox. The actor owns this, not its clients.
pub struct Mailbox<T>(mpsc::Receiver<T>);

/// Cloneable sending half of a bounded mailbox.
pub struct MailboxHandle<T>(mpsc::Sender<T>);

/// Receiving half of an unbounded mailbox, for existing non-blocking control paths.
pub struct UnboundedMailbox<T>(mpsc::UnboundedReceiver<T>);

/// Cloneable sending half of an unbounded mailbox.
pub struct UnboundedMailboxHandle<T>(mpsc::UnboundedSender<T>);

/// Create a bounded mailbox. A zero capacity panics, as with Tokio channels.
pub fn channel<T>(capacity: usize) -> (MailboxHandle<T>, Mailbox<T>) {
    let (sender, receiver) = mpsc::channel(capacity);
    (MailboxHandle(sender), Mailbox(receiver))
}

/// Create an unbounded mailbox. Notifications enqueue synchronously.
pub fn unbounded_channel<T>() -> (UnboundedMailboxHandle<T>, UnboundedMailbox<T>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (UnboundedMailboxHandle(sender), UnboundedMailbox(receiver))
}

impl<T> Clone for MailboxHandle<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Clone for UnboundedMailboxHandle<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxHandle<T> {
    /// Wait for capacity and enqueue a message; this does not await its handling.
    pub async fn send(&self, message: T) -> Result<(), MailboxError> {
        self.0.send(message).await.map_err(|_| MailboxError::Closed)
    }

    /// Enqueue a request and await its explicit reply. Dropping this future after
    /// enqueueing does not retract the message or cancel server-side work.
    pub async fn request<R>(&self, build: impl FnOnce(Reply<R>) -> T) -> Result<R, MailboxError> {
        let (reply, received) = oneshot::channel();
        self.send(build(reply)).await?;
        received.await.map_err(|_| MailboxError::ReplyDropped)
    }
}

impl<T> UnboundedMailboxHandle<T> {
    /// Enqueue without waiting for capacity or server-side handling.
    pub fn send(&self, message: T) -> Result<(), MailboxError> {
        self.0.send(message).map_err(|_| MailboxError::Closed)
    }

    /// Enqueue a request and await its explicit reply. Caller cancellation does
    /// not retract the message once it has been enqueued.
    pub async fn request<R>(&self, build: impl FnOnce(Reply<R>) -> T) -> Result<R, MailboxError> {
        let (reply, received) = oneshot::channel();
        self.send(build(reply))?;
        received.await.map_err(|_| MailboxError::ReplyDropped)
    }
}

impl<T> Mailbox<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.0.recv().await
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Stop accepting messages; already queued messages can still be drained.
    pub fn close(&mut self) {
        self.0.close();
    }
}

impl<T> UnboundedMailbox<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.0.recv().await
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Stop accepting messages; already queued messages can still be drained.
    pub fn close(&mut self) {
        self.0.close();
    }
}
