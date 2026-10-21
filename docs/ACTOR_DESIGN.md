# Typed actor mailboxes

Implemented in `actors/` (runtime) and `actors_macros/` (proc macro).

The enum is the protocol. The macro generates a typed client, not an actor,
dispatcher, task, or lifecycle framework. Servers still receive and match enums,
then explicitly send replies.

```rust
use actors::mailbox;
use tokio::sync::oneshot;

#[mailbox(client = ManagerClient)]
enum Command {
    IsBusy {
        session: u64,
        #[reply]
        reply: oneshot::Sender<bool>,
    },
    SessionUpdated {
        session: u64,
    },
}

// let (handle, mut mailbox) = actors::channel(256);
// let client = ManagerClient::new(handle);
// client.is_busy(session).await?;          // Result<bool, MailboxError>
// client.session_updated(session).await?; // Result<(), MailboxError>
```

## Generated surface

- A client wraps a `MailboxHandle<Command>`, a newtype over Tokio's mpsc sender.
- `Mailbox<Command>` wraps the receiver; the server calls `recv()` and matches.
- Client visibility follows the enum; `new(handle)` and `Clone` are provided.
- Each variant generates a snake_case method. Non-reply fields become arguments.
- Only a field explicitly marked `#[reply]` enables RPC. It must be a
  `oneshot::Sender<T>` (qualified paths allowed); one reply per variant.
- Reply methods return `Result<T, MailboxError>`. If `T` is itself a `Result`,
  it stays nested: transport errors and application errors are separate.
- No-reply methods finish when enqueued, not when the server processes them.

`#[mailbox(client = ControlClient, unbounded)]` selects an
`UnboundedMailboxHandle` / `UnboundedMailbox` from `actors::unbounded_channel()`.
Unbounded notifications enqueue synchronously; reply methods still await replies.
This preserves the existing non-blocking worker interrupt path.

## Errors and cancellation

- `MailboxError::Closed`: the message could not be enqueued.
- `MailboxError::ReplyDropped`: the message was enqueued, but its reply sender was
  dropped without a reply. This does not establish whether work completed.
- Dropping a waiting client future does not retract an already-enqueued request.
- Receivers expose `recv`, `try_recv`, `is_closed`, and `close`. Explicitly closing
  a mailbox rejects new messages while allowing queued messages to drain.

There are no automatic timeouts, retries, cancellation, spawning, or supervision.
Those decisions remain explicit in the application.

## Initial limits

Non-generic enums with named-field or unit variants. Reply sender type aliases,
conditional fields, and `Self` field types are unsupported; use explicit enum/type
names. Enum and variant conditional gates are preserved. Use the dependency name
`actors` in consuming crates.

## psa_server integration

- `ManagerCommand` generates a bounded `ManagerClient` for the global supervisor.
- `AgentManagerMessage` generates an unbounded `AgentClient` for worker controls.
- `RuntimeHandle` retains the public domain-oriented API: it delegates to the
  generated client, maps transport failures to `RuntimeError::Unavailable`, and
  unwraps application replies for callers.
- Worker boundaries and consumption acknowledgments use generated client methods.
- Existing receive matches, reply sends, queues, recovery, and shutdown ownership
  remain explicit and unchanged.

Publication broadcasts, SSE shutdown watches, and per-invocation cancellation
oneshots are not actor mailboxes and remain as they are.
