# PSA server foundation

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
Ctrl-C or SIGTERM drains HTTP requests and closes the SQLite pool. No provider
credentials are needed. `/health` is currently the only HTTP endpoint; sessions
and events are accessed through the Rust library. Agent/tool execution and queues
are not implemented by this foundation.

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
`new_state: Option<AgentState>`. `new_state` is `Some` only if the batch changes the
current state; these changes remain staged until the cursor is committed.

```rust,no_run
use psa_server::{
    agents::{
        NewAgent,
        agent_context::AgentContext,
        events::{EventId, EventRange, EventType, Message, MessageType, NewEvent},
    },
    db::Database,
    sessions::{NewSession, ForkPoint, SessionFork, SessionUpdate},
};
use llm_api::{api::RequestConfig, providers::{Effort, Model, ModelConfig, ProviderType}};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let db = Database::open("psa.sqlite3").await?;
let mut cursor = db.cursor().await?;
let session = cursor.create_session(NewSession {
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
// Future execution calls provider.generate(&request.history, &request.input, &request.config).

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
request contains this history and an empty separate `ModelTurnInput`; direct
`llm_api` callers can still supply additional input to `generate`.

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
messages using the same rich-content-first rule, preserving delivery and metadata.

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
