# actors

Small typed mailboxes over Tokio channels. No actor trait, generated dispatch,
background tasks, or lifecycle policy.

```rust
use actors::mailbox;
use tokio::sync::oneshot;

#[mailbox(client = CounterClient)]
enum Command {
    Add { amount: u64 },
    Read {
        #[reply]
        reply: oneshot::Sender<u64>,
    },
}

# async fn example() -> Result<(), actors::MailboxError> {
let (handle, mut mailbox) = actors::channel(16);
let client = CounterClient::new(handle);
let task = tokio::spawn(async move {
    let mut count = 0;
    while let Some(command) = mailbox.recv().await {
        match command {
            Command::Add { amount } => count += amount,
            Command::Read { reply } => { let _ = reply.send(count); }
        }
    }
});
client.add(2).await?;
assert_eq!(client.read().await?, 2);
drop(client);
task.await.unwrap();
# Ok(())
# }
```

- `#[reply]` marks the one reply sender. Other named fields become method arguments.
  Unannotated oneshots remain normal arguments, not implicit RPC replies.
- Generated clients have the enum\'s visibility, a `new(handle)` constructor,
  `Clone`, and snake_case methods corresponding to variants. Variant doc comments
  are copied to their generated client methods.
- Notifications complete when enqueued, not when processed. RPC methods await the
  actor\'s explicit reply. Application `Result` replies remain nested inside the
  transport `Result`.
- `MailboxError::Closed` means enqueueing failed. `ReplyDropped` means the request
  was enqueued but its reply sender disappeared without responding.
- `#[mailbox(client = ControlClient, unbounded)]` selects `unbounded_channel()`.
  Unbounded notifications are synchronous; RPC methods still await replies.
- Receivers expose `recv`, `try_recv`, `is_closed`, and `close`. Dropping all client
  handles closes the channel. Closing a receiver permits draining queued messages.
- Caller cancellation after enqueueing does not cancel the actor operation.
  Timeouts, shutdown, retries, and supervision remain application decisions.

The initial macro supports non-generic enums with named-field or unit variants.
Use the dependency name `actors` in consuming crates.
