# Initial event persistence and request rendering

## Scope

Implement the storage and replay foundation in `psa_server`: SQLite migrations,
sessions containing multiple agent streams, atomic event appends and session
forks, reconstruction of model requests, and a CLI that starts the server.
This document is the implementation plan; it does not implement the machinery.

Agent loops, inference execution, tool execution, probes, lifecycle hooks,
pending-message queues, and client session/event endpoints are future work.
Expose a usable asynchronous Rust API for sessions, agents, and events now.
The initial HTTP server only needs a health endpoint and graceful shutdown.

All new error types must use `thiserror`, including database, configuration,
projection, and request-readiness errors. Preserve underlying errors as sources
where applicable.

Preserve the agreed transcript semantics:

- Events are immutable. Current state and request history are projections.
- Messages enter the event log when lifecycle hooks consume them, not when
  submitted to a pending queue. Pending-queue durability is a separate feature.
- Tools are added within a context, in insertion order. They are not removed or
  reordered until `NewContext`.
- `NewContext` clears the model-facing context, configuration, and tool set.
  It does not erase the transcript, session metadata, agent identity, or lifecycle
  status. Context reconstruction requires a subsequent `Model` event.
- Compaction appends `NewContext`, configuration/tool definitions, `Compaction`,
  and transformed retained messages. Retained messages have no fabricated
  inference markers. The whole replacement is one atomic append.
- Streaming drafts are transient. Persist finalized response items and the
  inference end together when execution is implemented.
- Normal cancellation lets each tool finish its cancellation policy and return
  a persisted success/error result. Storage does not execute or retry tools.

## Domain model and ownership

Add `sessions.rs` and `agents.rs` for high-level types. Keep transcript types in
`events.rs`; add the missing `EventType` payload to `Event`.

Use distinct newtypes for `SessionId`, `AgentId`, and `EventId`. `EventId` is the
logical sequence number in one agent's visible stream, not a SQL primary key.
An event reference is `(AgentId, EventId)`, resolved in a specified session view.
Inherited events retain their physical owner agent; returned events must make
the source agent discoverable without pretending they were copied into a child.

A session owns zero or more agent streams. A child session owns a new child agent
for every source agent visible at its fork point, including agents with no events.
Each child agent points to its source agent and inherits a bounded prefix of that
stream. It is a new stream identity, not an alias that writes into the parent.
There is no independent single-agent fork operation in this first implementation.

Session metadata initially includes name, working directory, creation/update
timestamps, and immutable parent/fork metadata. Agent metadata includes name and
creation/update timestamps and the current `AgentState`; use the session's working
directory initially. New agents default to idle. Appending events atomically
updates the cached state from the last `StatusChange` in the batch; forks inherit
the state visible at their event cutoff.
Represent working directories as `PathBuf` in Rust and explicitly validated UTF-8
text in SQLite. Reject unsupported paths rather than using lossy conversion.
Do not require the directory to exist on the server; it may later refer to a probe.

`update_session` updates mutable metadata only. It cannot reparent a session,
move a fork point, rewrite events, or change the stream counter. Forks snapshot
the source's current name/working directory unless overridden; this does not
claim to recover historical metadata as of an old transcript event. Any future
metadata that changes the actual prompt must have a recorded request/event value.

No session/agent deletion or history pruning in this phase. Parent links are
immutable and can only point at existing records, preventing cycles through the
high-level API.

## Two logical orderings, separate from SQL identity

Multiple streams need a shared ordering to define a coherent session-wide fork.
Use these three distinct values:

| Value | Meaning |
| --- | --- |
| `events.event_pk` | Private SQLite `INTEGER PRIMARY KEY`, used by payload tables. |
| `events.event_seq` | Public `EventId`, sequential within an agent's visible lineage. |
| `events.session_position` | Branch-wide ordering across all agents, used for coherent forks. |

The branch position is internal storage metadata, not another primary key. Its
allocator lives on `sessions`, so concurrent appends to different agents still
have one order. Agent creation also consumes a position; otherwise a fork at an
old event could incorrectly include agents created after that event.

Each root agent starts with `first_event_seq = next_event_seq = 1`. Forking an
agent after its visible event `K` creates a child with
`first_event_seq = next_event_seq = K + 1`. Its inherited tail is `K`, even before
it owns an event. Empty prefixes use `K = 0`. Different sibling streams may use
the same event sequence numbers; `(agent_id, event_seq)` is unique.

A root session starts at position 1. Forking a session through position `P`
creates a child with `first_position = next_position = P + 1`. All new child
events and newly created agents get positions above `P`; siblings may reuse
positions. Do not use timestamps, SQLite row IDs, or the greatest event sequence
from one agent as a session-wide cutoff.

For example:

```text
Session S: agent A created @1; agent B created @2
           A event 1 @3; B event 1 @4; A event 2 @5; B event 2 @6

Fork S through A event 2 (@5) -> session T
  A' inherits A events 1..2; first local event sequence = 3
  B' inherits B event 1;    first local event sequence = 2
  T's first local session position = 6
  B event 2 @6 in S is not visible in T.
```

## SQLite schema and migrations

Put SQL migrations in `psa_server/migrations/`. Start with a migration creating
the session/agent trees, event envelope, all current payload tables, constraints,
and indexes. Keep the migration as the authoritative DDL; do not also maintain
a duplicate schema SQL file. Put low-level Rust row schemas in `db/schema.rs`.

### Core tables

`sessions`:

- `id INTEGER PRIMARY KEY`.
- `parent_session_id INTEGER NULL REFERENCES sessions(id)`.
- `first_position INTEGER NOT NULL CHECK (first_position >= 1)`.
- `next_position INTEGER NOT NULL CHECK (next_position >= first_position)`.
- `name TEXT NOT NULL`, `working_directory TEXT NOT NULL`.
- `created_at_us INTEGER NOT NULL`, `updated_at_us INTEGER NOT NULL`.
- A root must have `first_position = 1`. A child inherits source positions
  through `first_position - 1`. Parent and fork fields are immutable.
- Index `parent_session_id` for listing children.

`agents`:

- `id INTEGER PRIMARY KEY`.
- `session_id INTEGER NOT NULL REFERENCES sessions(id)`.
- `parent_agent_id INTEGER NULL REFERENCES agents(id)`.
- `first_event_seq INTEGER NOT NULL CHECK (first_event_seq >= 1)`.
- `next_event_seq INTEGER NOT NULL CHECK (next_event_seq >= first_event_seq)`.
- `created_position INTEGER NOT NULL CHECK (created_position >= 1)`.
- `name TEXT NOT NULL`, `created_at_us INTEGER NOT NULL`,
  `updated_at_us INTEGER NOT NULL`.
- `state TEXT NOT NULL DEFAULT 'idle'`, checked against the lifecycle states.
- Root streams have `first_event_seq = 1`.
- `created_position` means the logical agent's original creation position in the
  branch. A forked agent copies this value; creating the fork representation is
  not a new agent-birth position. SQL timestamps can record the new row's creation.
- Index `session_id` and `parent_agent_id`; permit at most one child of a given
  parent agent in a given session.
- Creation/fork operations validate that agent ancestry matches session ancestry.

`events`:

- `event_pk INTEGER PRIMARY KEY`.
- `agent_id INTEGER NOT NULL REFERENCES agents(id)`; no `session_id` column.
- `event_seq INTEGER NOT NULL CHECK (event_seq >= 1)`.
- `session_position INTEGER NOT NULL CHECK (session_position >= 1)`.
- `timestamp_us INTEGER NOT NULL`.
- `kind TEXT NOT NULL`, constrained to known event kinds.
- `UNIQUE (agent_id, event_seq)` and `UNIQUE (event_pk, kind)`.
- Index `(agent_id, session_position)` for historical stream cutoffs.
- The shared session allocator guarantees unique positions across a session's
  owned agents. Test that invariant; do not duplicate session ownership on events
  solely to express it as another unique index.

There is one payload table per `EventType`, including the otherwise empty
`NewContext` marker. Every payload has `event_pk` as its primary key and a foreign
key to the global envelope. Use a checked fixed `kind` column and a composite
foreign key `(event_pk, kind) REFERENCES events(event_pk, kind)` to prevent a
payload from belonging to the wrong envelope kind.

The write operation inserts exactly one matching payload for each envelope in
the same transaction. Reads reject missing payloads, unknown discriminators, and
invalid stored values as corruption; they must not silently omit events. The
foreign keys enforce ownership and kind, while transaction code establishes
payload existence before commit.

### Payload tables

| Table | Main fields beyond `event_pk` and fixed `kind` |
| --- | --- |
| `event_status_change` | Status discriminator; nullable reason. |
| `event_model` | Provider, API model name, reasoning effort. |
| `event_new_context` | None. |
| `event_message` | Message type, nullable delivery mode, content blocks (JSON array), replay metadata. |
| `event_thinking` | Structured provider reasoning/replay payload. |
| `event_tool_definition` | Stable tool ID; resolved definition snapshot. |
| `event_tool_call` | Call ID, tool name, arguments as the original string; replay metadata. |
| `event_tool_result` | Call ID, content blocks (JSON array), error flag, metadata. |
| `event_compaction` | Summary/replay payload and compaction usage. |
| `event_log` | Error flag and message. |
| `event_inference_result_start` | Inference ID and actual model/request-settings snapshots. |
| `event_inference_result_end` | Inference ID, outcome/stop reason, error details, duration, usage, response metadata. |

Use columns for scalar domain fields and explicit stable text discriminators for
enums. Use JSON text for genuinely structured provider data, JSON Schema, and
provider metadata, with JSON validity checks. Avoid one opaque JSON blob for the
entire event. Do not use Rust enum debug strings as a storage format.

Store times as checked signed Unix-epoch microseconds in SQLite and convert to
UTC `OffsetDateTime` at the operation boundary. Document microsecond precision;
normalize returned newly appended events to the same precision as reloaded ones.
Use checked nonnegative microseconds for elapsed durations and checked integer
conversions for IDs/token counts. SQLite integers are signed 64-bit, even when
the public Rust value is `u64`. Reject overflow rather than casting.

Before implementing the codec, make the small event-schema changes needed for
lossless request reconstruction: preserve provider attribution and item metadata
in reasoning/native replay data; retain completion versus incomplete stop reasons
and response metadata; distinguish unreported usage from zero. Prefer existing
`llm_api` content/metadata types where appropriate. Do not pretend the current
`ProviderItem { data: String, encrypted: bool }` alone is sufficient for replay.
Keep request-level usage on the result-end marker rather than copying it onto
every output item. These changes belong in the domain and payload codecs, not
in assumptions hidden inside the renderer.

### Connection and build setup

Use SQLx with SQLite, Tokio runtime, macros, and migrations; select a compatible
released version and matching `sqlx-cli` when implementing. No Postgres dependency
or TLS feature is needed for this database.

Open a small `SqlitePool` with file creation enabled, foreign keys explicitly
enabled, WAL journaling, a finite busy timeout, and `synchronous = FULL` for the
initial durable log. All operations use async SQLx; filesystem work uses Tokio.
SQLite's worker threads are an implementation detail, not a reason to introduce
blocking application database calls. SQLx exposes these connection settings in
[SqliteConnectOptions](https://docs.rs/sqlx/latest/sqlx/sqlite/struct.SqliteConnectOptions.html).

Use `query!`/`query_as!` with explicit columns and concrete low-level row structs.
Keep SQL literal and statically checkable, including the lineage query. Handle
SQLite nullability explicitly; only override inferred nullability when justified
by a constraint or query invariant. Prefer a few explicit payload queries over
dynamic table names or interpolated SQL. Batch loads by event kind rather than
one payload lookup per event; a fixed query per kind over each lineage segment
is sufficient initially.

Create a disposable development database from migrations, prepare query metadata,
and commit workspace-root `.sqlx/` metadata. Normal offline builds must not depend
on the developer's real PSA database. Check prepared metadata against a freshly
migrated scratch database in CI. See
[SQLx query macros and offline mode](https://docs.rs/sqlx/latest/sqlx/macro.query.html).

Embed migrations with `sqlx::migrate!` and run them before exposing the server.
Add `psa_server/build.rs` to watch the migration directory so adding a migration
rebuilds the embedded migrator, as described in
[SQLx migration recompilation](https://docs.rs/sqlx/latest/sqlx/macro.migrate.html).

## High-level asynchronous database API

Keep SQL, transactions, and low-level row types private under `psa_server/src/db/`:

```text
db/
  mod.rs          Database, DatabaseCursor, pool initialization, migrations
  schema.rs       Low-level SQLite row types and stored discriminators
  sessions.rs     Session create/update/list and whole-session forks
  agents.rs       Agent create/get/list and ancestry operations
  events.rs       Atomic append and coherent stream reads; payload conversion
  error.rs        Domain errors, invalid stored data, SQLx error wrapping
```

Expose operations in domain terms; avoid a generic repository framework:

```rust
create_session(new: NewSession) -> Session
get_session(id: SessionId) -> Session
list_sessions(parent: Option<SessionId>) -> Vec<Session>
update_session(id: SessionId, update: SessionUpdate) -> Session
create_agent(session: SessionId, new: NewAgent) -> Agent
get_agent(id: AgentId) -> Agent
list_agents(session: SessionId) -> Vec<Agent>
fork_session(source: SessionId, fork: SessionFork) -> ForkedSession
add_events(agent: AgentId, events: &[NewEvent]) -> AddEventResult
load_events(agent: AgentId, range: EventRange) -> Vec<Event>
```

All operations live on `DatabaseCursor`, take `&mut self`, and return
`Result<_, DbError>` asynchronously. `Database::cursor()` starts a SQLx-managed
`BEGIN IMMEDIATE` transaction. Explicit `commit` and `rollback` consume the
cursor; dropping it rolls back. Keep cursors short lived, including for reads,
and release them before external work or closing the pool. Multi-step mutations
use savepoints to undo an individual failed operation without discarding earlier
successful work in the same cursor.

`NewEvent` contains a timestamp
and payload, without a caller-assigned sequence or SQL key. `EventRange` has an
exclusive `after` and optional inclusive `through`, plus a bounded page size.
Fork points support `Head` and `ThroughEvent { agent_id, event_id }`; require the
agent to belong to the source session, and resolve its inherited events correctly.
Return the source-to-child agent ID mapping from a session fork.

`AddEventResult` contains the assigned `events` and `new_state: Option<AgentState>`.
The latter is set only when the last state-change event differs from the current
agent state. Empty batches, batches without state changes, and batches ending in
the original state return `None`. Results are staged until the cursor is committed.

`list_agents` returns the session's own agent representations, not all historical
ancestor rows. No raw SQL rows or database handles escape through this API.
Keep request rendering above the database layer.

### Atomic appends

For a nonempty append, start a savepoint in the cursor's immediate transaction
before reading either allocator. Read the target agent and
owning session, reserve consecutive agent event sequences and shared session
positions, insert all envelopes and payloads, and update both counters/timestamps
inside that savepoint. Release the savepoint before returning assigned events;
the caller must commit the cursor to persist them. Use the driver's transaction
abstraction so cancellation/error rolls back the savepoint and dropping the
cursor rolls back its entire transaction.

Do not allocate from `MAX(event_seq) + 1` outside a write transaction. Rollback
must undo both counter reservations and every payload/envelope. Empty appends
validate the agent exists and return an empty list without advancing counters.

Transaction atomicity applies to the submitted list; it does not mean every list
is already a complete runnable transcript. Storage must support result delimiters
and arbitrary historical prefixes. Semantic request readiness is checked
by rendering/validation, not by forbidding such prefixes in the database.

### Whole-session forks

Run the entire fork in a savepoint within the cursor's immediate transaction:

1. Resolve `Head` to the source session's current branch position, or resolve the
   specified visible event to its shared session position `P`.
2. Create a child session with parent = source and next/first position = `P + 1`.
3. Select source-session agent representations whose `created_position <= P`.
4. For each, find its greatest visible agent event sequence at/before `P`, or 0.
   Create a child agent with that source agent as parent, first/next event sequence
   = cutoff + 1, and the original logical `created_position`.
5. Copy current mutable metadata as documented, return every agent mapping,
   and release the savepoint. Do not copy inherited event/payload rows. The caller
   commits the cursor to persist the fork along with any other operations.

This includes empty streams, excludes agents born after the cutoff, and freezes
every parent-stream boundary together. Source writes after the fork cannot leak
into the child. A source session with no agents can also be forked at `Head`.

Allow a fork through an inherited event before the source session's own fork
point. In that case an agent cutoff may precede its immediate parent's first
local event; lineage resolution must skip empty segments rather than assuming
every ancestor contributes events.

### Coherent linear agent reads

Load the agent ancestry first, using a recursive CTE or a fixed parent query in a
single read transaction. Clamp the requested upper sequence to the captured
visible head. For that upper sequence `H`, each lineage row
contributes its locally owned interval `[first_event_seq, H]`; then move to its
parent with `H = min(H, first_event_seq - 1)`. Skip intervals with lower > upper.
Intersect with pagination bounds, read root-to-leaf, and order by event sequence.

The requested stream's captured `next_event_seq - 1` is its visible head,
including inherited events when it has no local events. Keep the ancestry,
captured heads, envelope reads, and payload reads in the same read snapshot.
For historical session-position queries, additionally cap each contributing
segment by `session_position <= P`. Do not return a parent's complete log and
filter by timestamps, and do not use global primary-key order as transcript order.

Validate lineage consistency and detect missing parents/cycles as stored-data
errors. An optional upper bound is a prefix read, so later pages can reuse the
same captured head while writers continue appending.

## Event projection into agent context and LLM requests

Add `agents/agent_context.rs` for a deterministic reducer. It takes ordered high-level
events, not SQL rows, and exposes both `apply_event` and a full replay constructor.
Start by replaying the full visible stream; persisted projection checkpoints and
optimized loading from the latest reset are future improvements.

The projection tracks:

- Agent lifecycle status and reason, independent of inference readiness.
- Optional current model configuration; absence blocks request preparation.
- Enabled tool IDs and resolved definitions in insertion order.
- Flat context items for `AgentHistory`, including compaction and retained messages.
- An optional open result block: ID, actual model/request snapshots, and buffered
  generated output items.
- The latest completed `ModelResponse`, retaining its original model attribution,
  outcome, usage, response ID, and provider metadata separately from history.
- Pending tool-call IDs and reasons a fresh request is not ready.
- The last applied event sequence, for incremental updates.

`prepare_request()` returns an owned value containing the complete flat
`AgentHistory` and `RequestConfig`, or a structured readiness error. It does
not send HTTP. The reducer also exposes inspectable state for prefixes that are
not runnable. Session working directory and other execution context remain
separate from the provider request values.

### Mapping rules

| Event | Projection effect |
| --- | --- |
| `StatusChange` | Update lifecycle state; no model content. |
| `Model` | Set current model and apply the existing model/provider replay-conversion policy. |
| `NewContext` | Clear model-facing history, tools, replay state, latest response, and open result block; require fresh model configuration. |
| `Message` | Map Human/Harness to user, Agent to assistant, System to system; delivery mode is transcript metadata. |
| `Thinking` | Preserve typed reasoning/provider replay data in its original order. |
| `ToolDefinition` | Append the snapshotted definition once; reject conflicting IDs or duplicate function names. |
| `ToolCall` | Preserve original arguments and track its call ID; never execute it. |
| `ToolResult` | Add the result/error flag and resolve its matching call ID. |
| `Compaction` | Add a textual summary as a user message with a fixed documented summary wrapper, or preserve an explicitly typed provider-native context item. |
| `Log` | No model content; available to transcript/UI readers. |
| `InferenceResultStart` | Open a generated-output block using the recorded model/request snapshots. |
| `InferenceResultEnd` | Close the matching result, append its items to flat context, and retain its `ModelResponse` snapshot separately. |

While a result block is open, assistant messages/reasoning/tool calls/native
items are buffered as its generated output. Reject prompt messages, tool results,
overlapping starts, mismatched ends, and model/tool changes inside the block.
Result delimiters describe generated output rather than request execution, and
the complete block can be persisted in one transaction after generation. An
empty, failed, or cancelled response still has both delimiters.

Outside a result block, append model-facing content directly to flat history.
Compacted assistant messages retain their role without needing a generated output
or synthetic inference markers. `ModelTurn` is removed from `llm_api`; its response
type is `ModelResponse` with the former output fields unchanged. The low-level
`generate(history, config)` API takes all prompt messages and tool results from
history; there is no separate request-input type or parameter.

Use the existing `llm_api` item conversion policy for model changes. Preserve the
original events and response snapshots; conversion only changes model-facing
context. The result-start model snapshot must match the active context model.

Snapshot per-request settings on `InferenceResultStart`. For the next request use the
explicit request configuration supplied to the projection/preparation operation,
defaulting to `RequestConfig::default()` initially. Do not infer future settings
from incidental diagnostic events. Keep credentials and endpoint selection in
server/provider configuration, outside stored model content.

### Prefixes, interruption, and compaction

Forking is allowed at any event, even midway through a result block or compaction
replacement. Raw persistence and replay inspection must work for that prefix.
`prepare_request` reports an open result block, missing configuration, unresolved
tool calls, or invalid replay content instead of silently completing a response.
Request readiness is distinct from the inherited `Running` lifecycle status.

Full compaction batches are atomically committed, but explicit historical forks
can cut inside the batch. An explicit post-reset `Model` and available content
determine what the selected prefix contains; do not pull later retained messages
from the parent to finish it. Reject malformed tool-call/result relationships
using `llm_api::api::validate_request` before exposing a prepared request.

No result after a crash/disconnect means a tool outcome is unknown. This phase
only reports outstanding calls. It does not add execution-start events, restart
jobs, fabricate cancellation results, or assume that a command had no effects.

## Mock tool registry

Keep the initial tool boundary small: use `BundleConfig::Noop` to install a
concrete mock tool identified by `ToolId::Noop`. Its `Tool::tool_definition()`
returns a stable name, description, and empty object argument schema. Construct
through the bundle; there is no standalone tool configuration. Do not build a
plugin system or executor.

At event construction, store the identity and definition snapshot together. Rendering uses the stored definition, so a later binary changing descriptions
or schemas cannot rewrite a persisted prompt. A fork inherits those definitions;
`NewContext` clears them, and re-adding a definition resolves a fresh snapshot.
Test the mapping and insertion order without actually running tools.

## Configuration and CLI startup

Use `directories::ProjectDirs::from("", "", "psa").data_local_dir()` as the
default application directory. On Linux this respects `XDG_DATA_HOME` and normally
resolves to `~/.local/share/psa`. Store both `config.toml` and `psa.sqlite3` there,
as requested, even though XDG also defines a separate config directory. On other
platforms use the library's corresponding local application-data directory.
See [ProjectDirs](https://docs.rs/directories/latest/directories/struct.ProjectDirs.html).

Add `config.rs` and `server.rs`, with a Tokio async main and Clap CLI:

```text
psa_server start [--data-dir PATH] [--config PATH] [--listen ADDRESS]
```

Use `--data-dir` for isolated instances and tests. Default config is inside that
directory; an explicit `--config` must exist. If the default config is absent,
create a documented template without overwriting an existing file. Malformed
config is an error, not a reason to silently regenerate it. Fail clearly if no
platform directory can be determined and no override was supplied.

Initial TOML contains `[server] listen = "127.0.0.1:8080"` and
`[database] path = "psa.sqlite3"`. Resolve a relative database path against the
selected data directory, independent of the process working directory. CLI
listen override wins over TOML. Use typed serde config structs with validation.
No inference credentials are required to start this storage-only server.

Startup: resolve paths, create directories, read/create and validate config, open
SQLite, apply migrations, build shared application state, bind a Tokio listener,
and serve a minimal `/health` endpoint with Axum. Log effective paths/address;
propagate errors with useful context. Shutdown on Ctrl-C and Unix SIGTERM,
stop accepting HTTP work, drain requests, and close the pool. Do not start an
agent loop just because a persisted projection says `Running`.

Expected dependencies: `sqlx`, `tokio`, `directories`, `clap`, `toml`, `axum`,
`tracing`/`tracing-subscriber`, and a small typed error implementation using
`thiserror`. Reuse existing `serde`, `serde_json`, `time`, and `llm_api`.
Only enable features needed for the chosen implementation.

## Implementation order and verification

1. Finalize domain envelopes, session/agent IDs, fork semantics, replay metadata,
   and the mock tool definition. Keep public domain types separate from SQL rows.
2. Add migrations, private row schemas/codecs, pool setup, and SQLx offline-query
   preparation against a disposable SQLite database.
3. Implement session/agent CRUD, atomic appends, and lineage reads. Then implement
   whole-session forks using both per-agent cutoffs and the shared position.
4. Implement the pure reducer and request preparation, including compaction
   context-only input, model switches, incomplete prefixes, and validation.
5. Add config/path resolution, async CLI startup, health endpoint, and shutdown.
6. Document local startup and migration/query-metadata regeneration commands.

Meaningful verification for the implementation:

- Migrate a fresh file, reopen it, and rerun migrations without data changes.
- Round-trip every event kind, optional fields, replay metadata, original tool
  arguments, and timestamp precision; reject invalid stored discriminators.
- Roll back an append with a failing payload and verify no envelopes, payloads,
  or counter advances remain. Exercise simultaneous writers through separate
  connections for one agent and for multiple agents in the same session.
- Test an empty session, empty agent streams, multiple agents, nested forks,
  a fork through inherited history before the immediate parent fork, sibling
  isolation, parent writes after a fork, and agents born after the fork cutoff.
- Verify a fork captures all streams at the selected event's shared position,
  produces an accurate agent mapping, and never exposes a half-created fork.
- Verify prefix/paginated reads across ancestry boundaries using a captured head.
- Replay user input, assistant output, reasoning, tool calls/results, usage,
  interruptions, and logs into exactly the expected history/config.
- Replay reset/compaction with retained assistant messages and no inference
  markers, then continue with a real inference without duplicating content.
- Compare full replay with incremental `apply_event` across the same events.
- Prepare requests without contacting a provider; exercise both adapter
  serialization paths for preserved provider metadata where applicable.
- Test config precedence/path resolution with explicit temporary directories,
  malformed-config behavior, and startup/migration/health/shutdown smoke tests.
- Run `cargo fmt --all`, relevant `psa_server` and affected `llm_api` tests,
  workspace compilation, an offline build, and SQLx prepared-query checks.
  Establish the current baseline first and report unrelated existing failures.

Use temporary file-backed SQLite databases for concurrency/WAL tests so tests
exercise real multi-connection behavior. No test should write into the user's
application-data directory, execute a tool, or require provider credentials.
