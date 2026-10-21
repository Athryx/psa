# PSA server

Start the server from the workspace root:

```sh
cargo run -p psa_server -- start
```

The Clap CLI supports `--data-dir PATH`, `--config PATH`, and `--listen IP:PORT`
after `start`. Use an isolated directory for development:

```sh
cargo run -p psa_server -- start --data-dir ./target/psa-dev --listen 127.0.0.1:8080
curl http://127.0.0.1:8080/health
```

The default directory is the platform's local application-data directory from
`directories::ProjectDirs`, normally `~/.local/share/psa` on Linux and honoring
`XDG_DATA_HOME`. Both `config.toml` and `psa.sqlite3` live there. The default config
is created only if missing; an explicit `--config` must already exist. Invalid
configuration fails startup without overwriting it.

```toml
[server]
listen = "127.0.0.1:8080"

[database]
path = "psa.sqlite3"
```

Relative database paths resolve against the selected data directory. The listen
CLI option overrides TOML. Startup applies embedded migrations before listening;
Ctrl-C or SIGTERM stops accepting commands, interrupts active agents, waits for
tool/hook cleanup and persistence, joins workers, closes SSE, drains HTTP, and
closes SQLite. Queued but unconsumed messages are memory-only and discarded.
No credentials are needed for storage/query operations or `/health`.

## HTTP client and credentials

This is a trusted-client API, with no authentication/authorization layer yet.
Keep it on loopback or behind a trusted authenticated transport; do not expose
agent execution, filesystem paths, or provider replay metadata publicly.

Place `auth.toml` beside the selected `config.toml` (including with `--config`):

```toml
[openrouter]
api_key = "YOUR_API_KEY"

# Optional; uses the existing llm_api adapter and its compatibility limitations.
#[openai_codex]
#access_token = "YOUR_ACCESS_TOKEN"
```

Create the file with owner-only access (`chmod 600 auth.toml` on Unix). The server
creates a missing file and adds `[psa].probe_key` when absent, preserving provider
credentials. Missing provider auth means no providers configured; malformed auth
fails startup without printing source excerpts/secrets. Login,
token refresh, and live credential reload are not implemented.

`psa_api` provides shared request/response types, route definitions, and an async
`Client`. Session views include runtime-derived `busy`; agent views include
lifecycle state and cumulative usage. Requests are accepted into bounded queues,
not necessarily persisted or executed at the time HTTP 202 is returned.

```rust,no_run
use psa_api::{Client, CreateSession, CreateAgent, AgentRequest, MessageDelivery};
use llm_api::{api::{ContentBlock, TextBlock}, providers::{Effort, Model, ModelConfig, ProviderType}};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = Client::new("http://127.0.0.1:8080")?;
let probe = client.list_probes().await?.into_iter().next().ok_or("connect a probe first")?;
let session = client.create_session(&CreateSession {
    probe_id: probe.metadata.id,
    name: "Example".into(), working_directory: "/work/project".into(),
}).await?;
let agent = client.create_agent(session.id, &CreateAgent {
    name: "Main".into(),
    model: ModelConfig { provider: ProviderType::Openrouter,
        model: Model::new("your-provider/model")?, effort: Effort::Medium },
    tools: vec![psa_api::ToolId::Noop],
}).await?;
client.submit_request(session.id, agent.id, &AgentRequest::Message {
    delivery: MessageDelivery::Queue,
    content: vec![ContentBlock::Text(TextBlock { text: "Hello".into(), annotations: vec![] })],
}).await?;
# Ok(())
# }
```

Routes are defined in `psa_api::routes`: create/list/get/update/fork sessions,
create/list/get agents, submit requests, and query session or agent events. Event
GETs return paged JSON or SSE with `stream=true`. `after` is an exclusive agent
EventId on an agent route, or shared session position on a session route;
`through` is inclusive. `limit` accepts 1–10,000. JSON pages expose `head`,
`next_after`, and `has_more`. Session lists default to roots; `parent` lists
immediate children.

Use `Client::stream_events(session, optional_agent, query, last_event_id)` with
`include_pending=true` to receive provisional upserts, committed events with
optional promotion IDs, and removal messages. Upserts contain complete current
snapshots with stable IDs/revisions, not independently persisted token deltas.
The initial `Snapshot` replaces pending client state after replaying the captured
durable head. Persist the stream's last SSE ID for reconnect; replayed committed
events must be upserted by viewed agent/event identity, not blindly appended.
The client intentionally leaves reconnect policy to callers: after EOF or
`ResyncRequired`, open another stream using that durable ID. Headers are supported
by using `Client::with_http_client` with a configured reqwest client.

One global session-manager task supervises lazy per-agent workers. Workers get
fresh context on restart, run `before_request` (currently no-op), infer/persist,
run `after_request` (`StopWithoutTools`), and resolve every committed tool call.
The hook emits Idle for responses without tool calls; the loop follows persisted
state. Compaction, probe execution, and custom agent definitions remain future
work. Tools/hooks receive the same `Cancellation` wrapper and choose their own
cancellation grace periods. Orphaned tool calls are repaired with interrupted,
unknown-outcome results, never automatically retried. All model-facing harness
strings are built in `src/prompts.rs`.
Incomplete tool calls with terminal error results are rendered as ordinary
context on replay, retaining the original call/result events for audit rather
than forwarding an unfinished native tool call to the provider.

## Storage and replay API

`db::Database` manages the pool and migrations. Call `db.cursor().await?` to get a
`DatabaseCursor`; all session, agent, and event operations take `&mut self` on the
cursor. Call `commit().await?` to persist changes or `rollback().await?` to discard
them. Dropping the cursor also rolls back. Both methods consume the cursor.

A cursor begins an immediate transaction, including for reads. Keep cursors short
lived and release them before model/tool/network work or closing the database.
Multiple operations share one coherent snapshot and can be committed atomically.
Multi-step writes use savepoints: a failed operation rolls back its own changes,
so callers can catch the error and still commit earlier successful operations.
SQL row types and the pool are private. All new errors use `thiserror`.
Each `Agent` exposes its current `state`. New agents start idle; event appends
atomically update this field using the last `StatusChange` in the batch. A fork
inherits the state visible at its cutoff, not its parent's later state. Migration
0005 backfills existing agents from their bounded event histories.
`add_events` returns `db::AddEventResult` with the assigned `events` and
`new_state: Option<AgentState>` and `new_usage: Option<AgentUsage>`. These are
updated snapshots only when changed; they remain staged until cursor commit.
Migration 0006 backfills cumulative token usage from inference results and
compaction calls through each ancestor cutoff. Counts are exact where known;
omitted per-call counts make that cumulative field unknown. Fork totals include
inherited usage, so adding totals across forks double-counts shared history.

Cursor helpers include `load_all_events`, `load_context_window` (inclusive of the
last NewContext), `load_events_since_last_inference` (latest start through head,
including tool results), and `load_last_inference_result` (`start`, `end`, `items`
strictly between the markers). Helpers page internally without truncation,
respect fork boundaries, and see staged writes. A latest result without an end
returns `DbError::IncompleteInference`, not an older completed result.

```rust,no_run
use psa_server::{
    agents::{
        NewAgent,
        agent_context::AgentContext,
    },
    db::Database,
    sessions::{NewSession, ForkPoint, SessionFork, SessionUpdate},
};
use psa_api::events::{EventId, EventRange, EventType, Message, MessageType, NewEvent};
use llm_api::{api::RequestConfig, providers::{Effort, Model, ModelConfig, ProviderType}};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let db = Database::open("psa.sqlite3").await?;
let mut cursor = db.cursor().await?;
let probe = cursor.list_probes().await?.into_iter().next().ok_or("connect a probe first")?;
let session = cursor.create_session(NewSession {
    probe_id: probe.metadata.id,
    name: "Example".into(),
    working_directory: "/work/project".into(),
}).await?;
let agent = cursor.create_agent(session.id, NewAgent { name: "Main".into() }).await?;

cursor.add_events(agent.id, &[
    NewEvent::new(EventType::Model(ModelConfig {
        model: Model::new("example-model")?,
        provider: ProviderType::Openrouter,
        effort: Effort::High,
    })),
    NewEvent::new(EventType::Message(Message::text(MessageType::Human, "Hello"))),
]).await?;

// Capture the head once so pagination replays a stable prefix during appends.
let head = EventId(cursor.get_agent(agent.id).await?.next_event_id.0 - 1);
let mut state = AgentContext::default();
while state.last_event_id < head {
    let page = cursor.load_events(agent.id, EventRange {
        after: state.last_event_id,
        through: Some(head),
        limit: 1000,
    }).await?;
    for event in page { state.apply_event(&event)?; }
}
let request = state.prepare_request(RequestConfig::default())?;
// Direct provider access calls generate(&request.history, &request.config).

let fork = cursor.fork_session(session.id, SessionFork {
    point: ForkPoint::Head,
    metadata: SessionUpdate { name: Some("Alternative".into()), ..Default::default() },
}).await?;
// fork.agents maps each source AgentId to its child agent, including empty streams.
# let _ = (request, fork);
cursor.commit().await?;
db.close().await;
# Ok(())
# }
```

Each event's public `EventId` is its agent-stream sequence, distinct from the
private SQL primary key. Reads of a fork include bounded ancestor segments,
retaining `source_agent_id`. Pagination uses exclusive `after`, inclusive
`through`, and a limit from 1 to 10,000. Use the requested child stream ID when
forking through an inherited event, rather than the event's physical source ID.

Session forks have a shared branch-position cutoff across every agent. Agent
creation consumes a position too, so an older fork excludes agents born later.
Parent appends cannot change a fork's inherited prefix. Forks can select any
visible event, including a prefix that cannot yet form a valid model request.

`AgentContext` is a pure reducer. `AgentHistory.items` is flat context: user prompts,
compaction summaries, retained assistant messages, and completed model outputs
appear in their original order. There is no `ModelTurn` pairing. The prepared
request contains this complete history and its request configuration.
`generate(&history, &config)` sends all history items once, in order; append new
messages and tool results to history before calling it.

Generated output is delimited by `InferenceResultStart` (ID and actual model/request
snapshots), output item events, and `InferenceResultEnd` (matching ID, terminal
outcome, usage, timing, and response metadata). Persist the entire block atomically
after generation, including both markers for an empty or failed response. These
markers describe a result, not a running request. Prompt messages and tool results
belong outside the block. A historical fork inside a result block is inspectable
but cannot prepare another request until completed or reset.

`ModelResponse` retains the provider output fields, separate from history.
`last_response()` exposes the latest completed response without changing its audit
attribution when context is converted to a different model. `NewContext` resets
model-facing history/configuration/tools and the response snapshot; it preserves
lifecycle status. Append the reset, model/tools, compaction summary, and retained
messages in one transaction. Replay never executes or retries anything.

Event scalar fields live in per-kind tables referencing the global `events`
table. JSON is used for structured provider data and schemas. Timestamps and
durations are stored at microsecond precision; timestamps decode as UTC.
Usage preserves unreported counts as `None`. Tool definitions are snapshotted,
so replay does not depend on a later binary's registry descriptions or schemas.
Tool results contain a single `content: tools::ToolResult` payload (an alias for
`Vec<ContentBlock>`), plus the call ID, error flag, and replay metadata. Content
blocks are stored as a JSON array and passed unchanged into history; there is no
separate display-text or replay-content field for tool results. Migration 0003
converts existing results, preserving rich content when present and wrapping
plain text in a text block otherwise.
Messages likewise have a single `content: Vec<ContentBlock>` payload, with
`Message::text` as a convenience constructor. Migration 0004 converts existing
messages using the same rich-content-first rule, preserving metadata. Legacy
delivery columns remain on disk for compatibility; scheduling delivery now lives
on API requests, not persisted messages.

Migration 0002 upgrades the previous inference payload tables and backfills model
snapshots through the bounded agent ancestry. Stored envelope tags remain
`inference_start`/`inference_end` for foreign-key compatibility; the Rust API and
payload table names use `inference_result_start`/`inference_result_end`. Legacy
start markers with no preceding visible model remain stored, but reading them
reports a missing snapshot rather than inventing model attribution.

## SQLx schema and offline builds

The migration directory is the authoritative schema. Normal builds use the
checked-in workspace `.sqlx/` query metadata and require no database connection.
To regenerate metadata after changing SQL or migrations, use a scratch database:

```sh
cargo install sqlx-cli --version 0.9.0 --locked --no-default-features --features sqlite
export DATABASE_URL="sqlite://$(mktemp -d)/psa-schema.sqlite3"
sqlx database create
sqlx migrate run --source psa_server/migrations
cargo sqlx prepare --workspace -- --all-targets
```

Do not point the schema/query preparation workflow at a real PSA database. Add a
new migration rather than modifying migrations already deployed to real users.
`build.rs` watches the migrations directory so new embedded migrations trigger a
rebuild. Commit `.sqlx/` updates alongside their query/schema changes.

For local and CI checks:

```sh
cargo fmt --all -- --check
SQLX_OFFLINE=true cargo test --workspace
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
# With DATABASE_URL pointing at the freshly migrated scratch database above:
cargo sqlx prepare --check --workspace -- --all-targets
```

Tests use temporary file-backed databases for WAL/concurrent-write behavior,
and loopback HTTP listeners. They do not use provider credentials or the default
application-data directory.

## Probes

See [the probe README](../psa_probe/README.md) for static builds, authentication,
probe discovery, and server-side remote operations. Sessions now require a
registered `probe_id`; forks inherit it. Agent-loop execution does not use probes yet.
