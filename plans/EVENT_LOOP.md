# Event loop and client API

Implemented in `psa_api` and `psa_server`; see `psa_server/README.md` for startup,
credentials, routes, client usage, and storage/query semantics. Compaction remains
unimplemented and pending requests remain memory-only as specified below.

## Scope

Implement `psa_api/API_OVERVIEW.md`: session/agent HTTP operations, finite event
queries and SSE, an async Rust client, lazy agent workers, hooks, and cancellable
tools. Keep database rows, credentials, queues, and task/channel types private to
`psa_server`. Probe APIs, configurable agent definitions, and real compaction are
later work.

## Shared types and HTTP

- Move IDs, `AgentState`, transcript payloads (`EventType` and its constituents),
  and the data-only `ToolId` into `psa_api`. These contain model/content data, not
  database or runtime machinery. Keep tool lookup/execution and convenience
  constructors needing the tool registry on the server. Re-export moved types
  from existing server modules where useful; do not maintain two event enums.
- Keep server `Session`/`Agent` records and define explicit API views. API session
  views include `busy: bool`; agent views include lifecycle state and cumulative
  token usage. Do not expose
  SQL primary keys or worker generations. Shared model/content types may depend
  on `llm_api`; provider replay metadata is visible only to trusted clients, never
  provider credentials. Client message requests contain content, not arbitrary
  transcript events or provider replay metadata.
- Define serializable requests, responses, query parameters, errors, and route
  constants/builders in `psa_api`; use the same definitions in Axum and the client.
  IDs are transparent integers; timestamps have an explicit UTC wire encoding.

| Method | Route | Operation |
| --- | --- | --- |
| POST / GET | `/sessions` | Create / list (optional parent filter) |
| GET / PATCH | `/sessions/{session_id}` | View / update metadata |
| POST | `/sessions/{session_id}/fork` | Fork all streams at head or selected event |
| POST / GET | `/sessions/{session_id}/agents` | Create / list agents |
| GET | `/sessions/{session_id}/agents/{agent_id}` | View agent |
| POST | `/sessions/{session_id}/agents/{agent_id}/requests` | Interrupt / queue / steer / resume |
| GET | `/sessions/{session_id}/events` | Session event query or SSE |
| GET | `/sessions/{session_id}/agents/{agent_id}/events` | Agent event query or SSE |

Every nested route validates session ownership. Request acceptance returns a
request ID and means queued, not persisted or executed. Invalid input is 400,
missing resources 404, invalid state conflicts 409, unavailable runtime 503;
internal failures are logged and return a sanitized typed error body.

## Task ownership and race-free retirement

- One session-manager task owns per-session active-agent maps, bounded pending
  request queues, and worker handles. One worker owns each active agent's
  `AgentManagerState`: session snapshot, agent record, context, and execution
  state. Inactive agents have no task. Keep short database transactions; never
  hold a cursor across inference, tool execution, hooks, or channel waits.
- The manager retains requests until the worker acknowledges consumption.
  Queue delivery is eligible at idle/interrupted boundaries; steer is also
  eligible between turns; resume is accepted only while interrupted. An accepted
  message is persisted only when consumed, in the same transaction as its
  transition to Running. Bound queue sizes and report overload explicitly.
  Recheck resume eligibility on consumption. If a worker dies before acknowledging
  an in-flight request, report unknown disposition rather than blindly replaying
  it; exactly-once retries require transactional request receipts.
- Workers announce `ReadyToStop` after persisting Idle/Interrupted and draining
  current work. The manager either assigns eligible queued work or authorizes
  graceful exit. Mark the worker as stopping; requests arriving afterward stay
  with the manager. Join the old worker before spawning its replacement.
  Include a private generation in worker notifications so stale messages cannot
  remove a replacement. Detect actual exit with `JoinSet::join_next_with_id`, not
  a separate exit-notification channel. Never remove a handle merely on a
  state-change message.
- Interrupt is cooperative, not `JoinHandle::abort`: cancel active work, collect
  results, persist them and Interrupted, then retire. Track joins/panics using a
  `JoinSet`; keep failure handling local to the affected agent. A draining worker
  may still make its session busy until cancellation/persistence completes.
- `busy` is derived from manager-owned admitted/running/draining work, not just
  persisted Running flags or the existence of an idle worker. On startup, repair
  orphaned Running states to Interrupted; do not restart inference automatically.
  Newly forked agents also do not auto-run; append an Interrupted transition when
  their inherited lifecycle state is Running. Fan out session metadata updates
  to active workers and reload metadata when starting inactive ones.
  Pending requests are memory-only for now. Accepted but unconsumed messages
  may be discarded on shutdown/restart; no pending-request table is needed.

## Cached agent usage

- Add `Agent.usage` and expose it in the API agent view. It is cumulative token
  usage across the agent's visible lineage, not monetary cost or current context
  size. Keep individual call usage on events as the source of truth. Use an
  aggregate type containing the numeric usage fields, not arbitrary provider
  metadata; cached input/reasoning output remain subsets, not extra total tokens.
- Like the cached lifecycle state, `DatabaseCursor::add_events` scans the appended
  batch and updates the agent's cached usage in the same transaction. Sum usage
  from each `InferenceResultEnd` and actual compaction call, including reported
  usage from failed/cancelled calls. Do not count provisional streaming snapshots,
  result-start delimiters, retained messages, or replayed context copies again.
- New agents start at zero. Add counts with checked arithmetic; an unreported
  count remains unknown rather than being treated as reported zero. An aggregate
  field is unknown if any counted call omitted that field. Reject overflow and
  roll back the whole append. `NewContext`/compaction must not reset totals.
- Extend `AddEventResult` with `new_usage: Option<AgentUsage>`, the updated total
  snapshot when changed, analogous to `new_state`. The worker assigns it to
  `state.agent.usage` only after cursor commit, alongside its state/context update;
  do not independently add the usage again in memory. Reload it when restarting
  a worker and include usage changes in client-visible agent updates.
- Add a migration/backfill for existing agents. Forks inherit usage only through
  each stream's fork cutoff, never the parent's current cached total; subsequent
  child appends update only the child. This is lineage usage, so summing multiple
  forked agents is not a deduplicated measure of total provider spend.
- Test multiple calls in one batch, missing/zero counts, compaction, cancellation,
  no-op batches, rollback/overflow, concurrent appends, reopen, historical/nested
  forks, and equality of committed database totals and worker memory.

## Cursor event-retrieval conveniences

Add methods on `DatabaseCursor`, all taking the viewed `AgentId`:

- `load_context_window`: all events from the most recent visible `NewContext`
  through the current head, including the reset marker. If there is no reset,
  return the entire visible history; an empty stream returns an empty vector.
- `load_events_since_last_inference`: all events from the latest visible
  `InferenceResultStart` through the current head, including both delimiters if
  present and subsequent tool results, messages, logs, and state changes. Return
  `None` if no inference exists. This is the latest result plus its aftermath,
  not the prompt/context that preceded the inference.
- `load_last_inference_result`: return `Option<InferenceResultEvents>` with
  exactly three fields: `start`, `end`, and `items`. Start/end retain their event
  envelopes and have `InferenceResultStart`/`InferenceResultEnd` kinds; items are
  the ordered events strictly between those markers, excluding later tool
  results and state changes. Validate matching inference IDs and delimiters.
  Return `None` if no inference exists; if the newest start has no visible end,
  report an explicit incomplete-result error, never silently return an older
  completed result or manufacture an end marker.

All searches use bounded agent ancestry and the cursor's transaction snapshot,
so inherited markers work across forks and later parent writes stay invisible.
The inference helpers search the visible lineage independently of context
resets; callers wanting only the current context can use its reset boundary.
Find boundaries from the newest lineage segments rather than decoding the full
log merely to locate a marker. Load complete selected ranges, internally paging
as needed; do not truncate at `load_events`' default/page-size limit. Add tests
for absent markers, empty streams, multiple blocks/resets, post-inference events,
incomplete fork prefixes, matching IDs, inherited boundaries, and large ranges.

## Agent execution, hooks, and tools

Replay the full bounded lineage when starting a worker and repair orphaned tool
calls before consuming input or preparing inference. Consume eligible input, run
the before-request hook, prepare history, infer while processing interrupts,
persist the complete inference result block, run the after-request hook, execute
completed tool calls, persist results, then consume steering and repeat. Committing
tool calls creates an obligation to resolve each call; do not wait for another
client request to start execution. The after-request hook decides normal
continuation by emitting lifecycle events; the loop only follows committed
`AgentState`, resolves outstanding tool calls, and consumes eligible messages.
There is no separate tools/no-tools "choose next step" branch in the loop.
Failure or interrupt preserves available output and transitions to Interrupted.

- In `hooks.rs`, define `Hook::run(&mut AgentManagerState, Cancellation)` with no
  separate database/context arguments. Expose a controlled `persist_events`
  method on state. Implement concrete hook structs and a dispatch enum
  implementing the trait; `Hooks` has one enum-valued field per lifecycle hook.
  Default `before_request` to a no-op: compaction remains unimplemented.
- Implement a default `StopWithoutTools` after-request hook. After a successful
  inference result commits, inspect its items (using the structured retrieval
  helper): if it contains tool calls, leave Running unchanged; otherwise persist
  `StatusChange(Idle)`. Do not override Interrupted from a failure/cancellation.
  The loop follows the resulting state: Running executes/resolves tool calls and
  continues; Idle/Interrupted consumes eligible queued input or requests manager
  retirement. Consuming input can transition Idle/Interrupted back to Running.
  Outstanding tool results must still be resolved even if a hook changes state
  or is cancelled; lifecycle transitions do not discard committed call IDs.
- Extend `Tool` with async execution accepting parsed arguments, execution
  context, and the same `Cancellation` parameter as hooks. Noop implements the
  complete contract.
- Define a shared server-side `Cancellation` wrapper with a private
  `oneshot::Receiver<()>` and latched cancellation state; do not expose the raw
  receiver in either trait. Provide `cancelled(&mut self).await` and
  `is_cancelled(&mut self)`; cancellation remains observable on repeated calls,
  and waiting must be cancellation-safe in `select!`. The worker retains the
  paired sender for each hook/tool invocation. Interrupt signals active
  invocations, then awaits their results without dropping the execution futures;
  dropping the sender also requests cancellation.
- Persist tool success/error content using the existing content-block alias;
  execute only final calls with valid arguments and registered tool names.
  Never execute a call before its event commits. Every committed call must have
  one terminal `ToolResult` before the next inference or clean worker exit:
  actual success/error, cancellation, timeout, or not-run error. Unknown tools,
  invalid arguments, and calls skipped because of interrupt/hook failure also
  receive persisted error results with their original call IDs. Start with
  sequential execution; cancel/await the active tool and resolve all unstarted
  calls as not run. A tool that finishes successfully during cancellation keeps
  its actual success result. Missing results after crashes/failed persistence
  are repaired on restart, not silently ignored.
- Startup recovery checks unmatched tool calls in the last inference response
  and any outstanding calls retained in the current context, using the agent's
  bounded visible lineage (including inherited calls in forks). Append error
  results saying execution was interrupted, plus Interrupted if needed, in one
  transaction before allowing inference. Do not rerun orphaned calls: a missing
  result cannot establish whether execution started or had side effects. Repair
  only unresolved call IDs; recheck under the transaction and make repeated
  recovery idempotent. Never alter ancestors or duplicate existing results.
  Incomplete calls paired with terminal error results render as ordinary context
  for future requests; their raw call/result events remain unchanged for audit.
- Both `Tool` and `Hook` expose `cancellation_timeout(&self) -> Duration` so each
  implementation defines its bounded cleanup grace period. Start the timer when
  cancellation is requested, not when execution begins; this is not a normal
  execution deadline. A timeout is explicitly an unknown execution outcome, not
  proof no side effect occurred; persist that outcome and do not silently retry.
  Hard abort is a last-resort fallback, followed by joining the task and recovery
  marking. Use `thiserror` for all new error types.
- Validate a cloned context with proposed persisted events, commit SQLite, then
  swap live state and publish. Current `save_events` mutates context before
  commit; fix that so projection/commit failures cannot diverge memory and disk.
- Centralize all harness-authored model-facing text in `psa_server/src/prompts.rs`
  as named text/content-building functions, not scattered literals or string
  constants. This includes system/hook/compaction prompts, tool descriptions,
  context-summary wrappers, and tool error results. Use builders such as
  `tool_interrupted()`, `tool_timed_out()`, and `tool_not_run()` for the canonical
  texts: "Tool execution was interrupted; its outcome is unknown and it may have
  had side effects.", "Tool cancellation timed out; its outcome is unknown and
  it may have had side effects.", and "Tool was not run because execution was
  interrupted." Use the last wording only when this worker knows the call never
  started; recovery uses the unknown-outcome wording. Operational logs and HTTP
  errors that are not supplied to the model need not live in this module.

## Persistent and provisional streams

- Use the event routes above with `stream=true` for SSE; otherwise return paged
  JSON. `include_pending=true` adds a snapshot of active provisional items.
  A session stream multiplexes all its agents; agent filtering happens at open.
- Stream envelopes identify viewed session/agent and physical source agent.
  Durable events also expose the existing session position. Session queries use
  that shared ordering across bounded ancestor streams; agent queries retain
  per-lineage `EventId` ordering. Do not merge streams by timestamp or event ID.
- Provisional items use a stable unique ID independent of database IDs and a
  monotonically increasing revision. Send nonpersistent upserts containing the
  current partial item (including thinking), then a promotion mapping that ID to
  committed event(s), with authoritative final content. Also support removal for
  abandoned drafts. Do not persist token updates; persistence does not imply
  successful completion. Publishing must never block the agent on a slow client.
- Subscribe before capturing a replay head and provisional snapshot, with a
  coordinator barrier covering commit/publication and snapshot revisions. Replay
  durable events through that head, then deduplicate buffered live notifications;
  skip draft updates superseded by the snapshot. Publish promotions only after
  commit. Keep a single ordered publication path per agent.
- SSE IDs encode a durable resume cursor, not provisional IDs. Honor
  `Last-Event-ID` (and a query cursor for nonbrowser clients). Reconnect replays
  durable events and replaces pending state with a fresh snapshot; do not claim
  exactly-once delivery. Bounded broadcast lag emits a resync indication and
  closes the connection instead of silently dropping durable events.
- Send keepalives. Shutdown sequencing is defined below; SSE must terminate
  before awaiting the final Axum drain so open streams cannot block shutdown.

## Credentials and graceful shutdown

- Read `auth.toml` alongside `config.toml` in the application data directory. Use
  a typed optional section for each authenticated provider and provider-specific
  credential fields, e.g. `[openrouter] api_key` and `[openai_codex] access_token`.
  Instantiate the runtime's provider registry from present sections; a missing
  file means no configured providers, and malformed credentials/config fail with
  a sanitized error. No login/token-refresh flow is added in this iteration.
- Keep auth server-only: never include secrets in API views, events, SQLite, or
  logs; redact debug output and parse errors. Do not overwrite existing auth
  files. Any file the server creates must be owner-only on Unix (`0600`).
- On a shutdown signal, stop admitting commands and mark the session manager
  shutting down so queues cannot start more inference or replacement workers.
  The manager interrupts every active worker, including those in hooks/tools.
  Discard unconsumed queued requests without persisting or executing them.
- Workers close inference streams while retaining partial output, signal tool
  cancellation, await cleanup using each implementation's grace period, and
  persist results and Interrupted. An already retiring worker still must finish
  persistence. Do not abort workers just because graceful shutdown was requested.
- The manager joins all workers before reporting shutdown complete. Publish final
  committed updates, close SSE streams, drain HTTP handlers, then close SQLite.
  Persistence failures must surface as shutdown errors, not successful cleanup.
  Forced process termination/crashes cannot provide this graceful guarantee.

## Implementation and checks

1. Shared API types/routes and async reqwest client, including typed SSE parsing.
   Fix existing message codec/tests still referencing removed `Message.delivery`;
   delivery belongs to request scheduling, not the persisted message payload.
2. Database session-position reads, ownership checks, context/inference retrieval
   helpers, cached agent usage with migration/backfill and append-result updates,
   and API-view conversion.
3. Session-manager supervision/queues and worker lifecycle; hooks/tools and
   inference via an injected provider registry (fake provider in tests), default
   `StopWithoutTools` after-request hook, no-op before-request hook (no compaction
   implementation), orphaned tool-call recovery, and centralized `prompts.rs`.
4. Provider auth loading, publication coordinator, provisional snapshots/promotions,
   Axum handlers, graceful shutdown, and client/server integration tests.

Test retirement/request races, no overlapping workers, stale notifications,
interrupts during inference/tools/hooks, commit failure, queue ordering, busy
transitions, restart recovery, ownership rejection, nested fork event ordering,
SSE replay/live handoff and lag/reconnect, promotion without duplicates, auth
validation/redaction, per-tool/hook cancellation deadlines, and shutdown with
active work/open streams (all committed before completion), and discarding
unconsumed queues on shutdown. Test cancellation between tool-call commit and
execution, mixed completed/unstarted calls, timeout results, interruption during
the after-request hook, crash recovery without rerunning side effects, idempotent
repair across nested forks, prompt builders, and hook-driven state changes with
and without tool calls/queued input. Run `cargo fmt --all`, workspace tests,
relevant Clippy checks, and refresh/check SQLx offline metadata for changed queries.

## Deployment assumption

- Initial deployment assumes trusted local clients. Authentication/authorization
  is required before exposing replay metadata, filesystem operations, or tool
  execution to remote clients; do not advertise this API as safe for untrusted
  access yet.
