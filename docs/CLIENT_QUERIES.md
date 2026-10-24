# Client queries

Tools and hooks can request client input without owning HTTP connections or a
pending-response registry:

```rust,ignore
let response = context.query_client(query, &mut cancellation).await?;
```

`query` is `psa_api::ClientQuery`, and the result is
`Result<psa_api::ClientQueryResponse, ClientQueryError>`. Context supplies the
invocation's session and agent IDs. This capability also works in offline-probe
lifecycle hooks. It does not require `Agent.interactive`; that field remains
metadata rather than an implicit permission or UI-availability gate.

The initial payload is `ClientQuery::AskQuestion`, containing ordered questions
with optional suggested choices. `ClientQueryResponse::AskQuestion` contains an
answer and optional notes for every question in order. Freeform/Other answers
are valid even when choices were supplied. Empty questions, blank question or
choice text, wrong answer counts, and blank answers are rejected. Validation
errors do not echo question/answer contents.

This feature supplies transport and runtime capabilities, NOT a native
`ask_question` tool. Tool authors still define their typed tool arguments,
format their model-facing result, and handle errors intentionally. Hooks can
propagate query errors through `HookError::ClientQuery`; cooperative cancellation
should be handled according to the hook's intended interruption behavior.

## Ownership and cancellation

The session manager owns one `ClientQueries` service for its runtime. A dedicated
workspace-`actors` actor owns pending requests and one-shot response senders.
Only the calling tool/hook awaits a human answer; the actor continues processing
other registrations, responses, cancellations, and shutdown messages.

A synchronous cancellation guard is installed before registration is enqueued.
Cancelling the invocation or dropping its query future removes the request, even
if registration is still queued or waiting for the publication gate. A dropped
HTTP/SSE connection does NOT cancel a query. Without any connected UI, requests
remain pending until a client connects, the caller cancels, or runtime shutdown
begins. There is no automatic deadline; callers can apply a timeout by wrapping
and dropping the query future.

At runtime shutdown, workers are interrupted before the query service is closed.
Closing the service rejects new queries, removes its outstanding UI projections,
wakes remaining callers with `ClientQueryError::Unavailable`, and joins the
actor. Synchronous owner Drop aborts its task as a fallback; graceful shutdown is
the path that emits removal notifications. Queries are not durable, restored on
restart, or copied into forks. Actual tool results are persisted by the usual
worker path; query transport itself adds no transcript events or agent status.

The registry permits 1,024 pending requests. It retains only the last 1,024
answered/cancelled IDs for duplicate-response diagnostics, without retaining
answer payloads. Older IDs eventually become unknown.

## Publication and clients

The actor updates a read projection under the existing `Publication` gate and
publishes into the same session/agent SSE streams used for other runtime state:

- `client_query_requested { query }`: upsert by query UUID.
- `client_query_removed { session_id, agent_id, query_id }`: remove by UUID.
- `snapshot { ..., queries }`: replace query state for that stream's scope.

Event endpoints use `Accept: text/event-stream` for SSE and
`Accept: application/json` for JSON; `EventsQuery` has no `stream` field.
`Client::stream_events()` selects SSE and optionally sends `Last-Event-ID`;
`Client::events()` selects JSON without that resume header.

`SessionUpdated` on these streams carries a full `Session` for both metadata
and runtime busy changes, ordered under the publication gate. There is no
separate `SessionBusy` message.

Nonstream event pages also include `queries`. These are current runtime state,
not historical events at a transcript cursor. Queries are included regardless of
`include_pending`, which continues to control provisional transcript events.
They do not advance the durable SSE cursor. Snapshot subscription and projection
capture share one gate, so reconnecting cannot lose an outstanding request in a
subscribe/snapshot race. A resync should rebuild query state from the snapshot.

Respond with the shared async client:

```rust,ignore
client.respond_to_client_query(session_id, agent_id, query.id, &response).await?;
```

The underlying route is
`POST /sessions/{session_id}/agents/{agent_id}/queries/{query_id}/response`.
A valid response returns 204. The first valid response wins; malformed or
semantically invalid responses do not consume the request. Wrong scopes or
unknown IDs return 404; recent answered IDs return 409; recent cancelled IDs
return 410. Unavailable or overloaded service returns 503. After tombstone
eviction or server restart, old IDs return 404.

As with the rest of the HTTP API, this is trusted-client transport, not a new
client authentication or exclusive-controller/claim protocol. Any client with
access to the correctly scoped endpoint may submit the first valid answer.
Wire examples and error codes are in `psa_api/API_OVERVIEW.md`.
