# Tool and hook interface

- At each agent-loop iteration, fetch the current probe handle using the session's
  probe ID. If unavailable, persist `Interrupted` and skip execution. Never retry
  disconnected operations automatically.

- Introduce a shared execution `AgentContext` for tools and hooks; rename the
  existing transcript projection to avoid the name collision. Expose:
  - Database-equivalent event-trace queries, with the current agent ID bound
    internally so callers cannot query another agent through this interface.
  - `save_events(...)`, delegating to worker persistence so projection, status,
    usage, and publication remain synchronized.
  - Current session and agent definitions.
  - The iteration's current probe handle.
  - A `Sessions` handle for session/agent operations.

- Hooks return `Result<(), HookError>`. Tools return `{ content, error: bool }`,
  not `Result`; the error flag is model-facing and does not control the agent loop.
  The worker separately constructs and persists every tool-response event,
  including cancellation/timeout results. Preserve cooperative cancellation and
  cleanup. A Result-based inner tool helper is future work.

- Tools/hooks decide whether mid-iteration probe failures should interrupt the
  agent by saving a status event. Do not automatically interpret tool error flags
  or probe errors as interruption; the next iteration still checks probe availability.

- In worker event persistence, omit status changes matching the current status,
  accounting for earlier transitions in the same batch. This prevents repeated
  interruption events from multiple tools reporting a disconnected probe.

- Add `agents/sessions.rs` exposing `Sessions`, replacing `RuntimeHandle`. Combine
  a database handle and session-manager client behind a consistent async API for
  session create/query/update/fork, agent create/query, requests/steering/resume/
  interrupt, and busy queries. Centralize validation and orchestration currently
  scattered across HTTP handlers; keep handlers as thin wire adapters. Keep queue
  and worker ownership in the session manager and avoid actor self-request deadlocks.

- Wire `ProbeRegistry` and `Sessions` into workers and migrate all tools/hooks to
  the shared context. Test missing/disconnected probes, status deduplication,
  trace persistence, separate tool responses, and HTTP/Sessions behavior parity.
  Run formatting and relevant tests.
