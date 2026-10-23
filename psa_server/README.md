# PSA server

Agent components live under `src/components/`: `bundles/` owns shared resources
and assembly, `tools/` implements model-facing tools, and `hooks/` implements
lifecycle callbacks. Shared invocation cancellation lives in
`src/components/cancellation.rs`. Tools and hooks can request typed client input
through `AgentContext::query_client`; see `../docs/CLIENT_QUERIES.md` for the
runtime-owned query registry, SSE snapshots, and response endpoint. Native Goal
and Subagents bundles are documented in `../docs/GOALS_AND_SUBAGENTS.md`.

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
use psa_api::{Client, CreateSession, CreateAgent, CreateAgentDefinition, AgentRequest,
    HookId, HookIds, MessageDelivery};
use llm_api::{api::{ContentBlock, TextBlock}, providers::{Effort, Model, ModelConfig, ProviderType}};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = Client::new("http://127.0.0.1:8080")?;
let probe = client.list_probes().await?.into_iter().next().ok_or("connect a probe first")?;
let session = client.create_session(&CreateSession {
    probe_id: probe.metadata.id,
    name: "Example".into(), working_directory: "/work/project".into(),
    model_config: ModelConfig { provider: ProviderType::Openrouter,
        model: Model::new("your-provider/model")?, effort: Effort::Medium },
}).await?;
let definition = client.create_agent_definition(&CreateAgentDefinition {
    name: "Assistant".into(),
    selectable: true,
    bundles: vec![psa_api::BundleId::FileTools],
    tools: vec![],
    hooks: HookIds {
        agent_create: vec![
            HookId::DefaultStartup, HookId::ToolDefinitions, HookId::Cwd, HookId::Skills,
        ],
        session_update: vec![HookId::SessionModelUpdate],
        after_request: vec![HookId::StopWithoutTools],
        ..Default::default()
    },
    ..Default::default()
}).await?;
let agent = client.create_agent(session.id, &CreateAgent {
    interactive: false,
    definition_id: definition.id,
    name: "Main".into(),
}).await?;
client.submit_request(session.id, agent.id, &AgentRequest::Message {
    delivery: MessageDelivery::Queue,
    content: vec![ContentBlock::Text(TextBlock { text: "Hello".into(), annotations: vec![] })],
}).await?;
# Ok(())
# }
```

Routes are defined in `psa_api::routes`: create/list/get/update agent definitions,
create/list/get/update/fork sessions, create/list/get agents, submit requests,
and query session or agent events. Event
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

### Definitions and hook bindings

This development schema requires a fresh database; existing databases and clients
using the former per-agent model/tools creation fields are not migrated.

`POST /agent-definitions` creates a definition; `GET /agent-definitions` lists
all immutable versions and `GET /agent-definitions/{definition_id}` fetches one.
`PUT /agent-definitions/{definition_id}` takes a full replacement, returning HTTP
201 with a new ID, incremented `version`, and `previous_version_id` pointing to
the source. `Client::update_agent_definition` wraps that operation. Omitted
optional/defaulted fields are cleared, not merged. Existing agents and forks
remain pinned to their original definition ID; edits do not reconfigure them.

Agent creation accepts `interactive`, defaulting to `false` when omitted from
JSON. It is creation-time metadata available to tools and hooks, not a runtime
behavior switch. The opt-in Goal bundle uses it to enable goal tools and policy.
Agent responses expose it, and forks inherit the source flag.

Sessions require `model_config`, which can be changed with `UpdateSession`.
A definition's optional `model_config` overrides the session model. Definitions
also store `description`, `selectable`, `skills`, `permitted_subagents`, and
`extra_system_prompt`. Skills locations are consumed only when `SkillsHook` is
explicitly configured. Metadata does not automatically select agents or inject
extra system prompts. The opt-in Subagents tool checks `permitted_subagents` when
creating children; the trusted-client administration API remains unrestricted. Unknown extra JSON fields are rejected, not treated as configuration.

Definitions also persist ordered `bundles`, defaulting to `[]`.
`BundleId::FileTools` installs `Read`, `Edit`, and `Write`; `BundleId::Bash`
installs `Bash`, `InteractBash`, and `CloseBash`. `Goal` installs interactive
`SetGoal`/`SetGoalStatus` plus stopping policy. `Subagents` installs group creation,
messaging, waiting, and listing backed by session-shared actor resources. These
tool groups require their bundle. See `../docs/GOALS_AND_SUBAGENTS.md` for setup,
fixed limits, append-only goal semantics, and checkpoint/recovery limitations. Bundles install in list order before explicit tools/hooks; independent
explicit configuration remains supported. Duplicate bundle IDs, tool IDs, or tool
names are rejected during component installation. The session manager retains
owners and instances by agent ID across worker retirement/failure. Forked agents
have independent resources; server restart does not restore runtime resources.
Shared session resources do not share tool instances or mutable hooks across
agents. Shutdown interrupts all workers, closes query/session coordination
services, then joins workers and cleans per-agent bundles in reverse order under
bounded timeouts. Probes stay
connected until cleanup finishes. See `../docs/BASH_TOOLS.md` for process tools. Migration 0009 defaults existing definitions to
`[]`; omitting bundles in a full replacement clears only the new version's list.

Each definition explicitly binds ordered `HookId` lists at `agent_create`,
`session_update`, `before_request`, and `after_request`. All lists default empty;
no global hooks are installed by default, though explicitly selected bundles
supply their own hooks. Built-ins:

- `DefaultStartup`: normally bound to `agent_create`; saves `NewContext`, the
  resolved `Model`, and a `System` message with the base assistant identity for a
  new root agent. It does not emit tool definitions. It skips forked agents
  entirely so inherited context and its model are not reset; an explicit session
  update can synchronize an inherited model afterward.
- `ToolDefinitions`: separately emits tool-definition events from the worker's
  tool instances, then separate `<tools>` and `<rules>` system messages for any
  tool-provided directions. Empty sections are omitted. Normally bound after
  `DefaultStartup` on `agent_create`; it also skips forks to preserve inherited
  tool definitions and prompt sections.
- `Cwd`: emits a `System` message containing `<cwd>\nSESSION_CWD\n</cwd>`, with the
  session working directory XML-escaped. It does not skip forks. Bind it to
  `agent_create` and optionally `session_update` to reflect directory changes.
- `Skills`: reads only explicit `AgentDefinition.skills` locations on the session
  probe. Relative locations resolve against the session's `working_directory`;
  locations can be Markdown (`.md`) files or directories containing `SKILL.md`.
  It emits a pi-style, XML-escaped catalogue containing skill names, descriptions,
  and paths, not skill bodies. Skills with `disable-model-invocation: true` are
  omitted; the first duplicate name wins. Missing or malformed skills produce
  warning `Log` events and are skipped. An empty catalogue emits no system
  message. A nonempty skills list requires a connected probe; a disconnected
  probe is an error rather than a missing-skill warning. Bind it after `Cwd` on
  `agent_create`; without this hook, skills locations are not consumed.
- `SessionModelUpdate`: normally bound to `session_update`; saves a changed
  session model unless the definition has its own model override. It does not
  emit `NewContext` or clear inherited history.
- `StopWithoutTools`: normally bound to `after_request`; saves Idle for responses
  without tool calls.
- `Noop`: does nothing.

One global session-manager task supervises lazy per-agent workers. Creation
callbacks still run for newly forked agents, but not on worker reload or server
recovery. Session-update callbacks run for every agent in an updated session,
starting idle workers as needed. Hooks run at worker boundaries, serialized with
inference, tools, and other hooks. Updates queued during worker retirement are
carried to its replacement. These notifications are in-memory, not a durable
retry queue. Creation returns before asynchronous startup hooks finish.

Workers replay their transcript on restart, run the configured `before_request`
hooks, infer/persist, run configured `after_request` hooks, and resolve every
committed tool call. The loop follows persisted state. Compaction and built-in
probe-executing tools remain future work. Tools/hooks receive the same
`Cancellation` wrapper and choose their own cancellation grace periods. Orphaned tool calls are repaired with interrupted,
unknown-outcome results, never automatically retried. All model-facing harness
strings are built in `src/prompts.rs`.
Incomplete tool calls with terminal error results are rendered as ordinary
context on replay, retaining the original call/result events for audit rather
than forwarding an unfinished native tool call to the provider.

## Tools, hooks, and session operations

Tools and hooks borrow `agents::agent_context::AgentContext` for each invocation.
It exposes `agent()`, `session()`, the pinned `definition()`, agent-runtime
`tools()`, `projection()`, the iteration's `probe()`, and `sessions()`.
`probe()` returns a `Result`: lifecycle hooks run even with an offline probe, and accessing it then returns `ProbeError::Disconnected`. Each session-update
hook sees the snapshot for that update. Trace methods mirror the database queries
with the current agent ID filled in: `load_events`, `load_all_events`, `load_context_window`,
`load_events_since_last_inference`, `load_last_inference_result`, and
`load_agent_envelopes`.

`save_events` persists and publishes auxiliary events through worker persistence,
keeping its projection/status/usage synchronized. Repeated status changes to the
same status are omitted, including duplicates within one batch. Started saves
finish even if their tool/hook times out, before the worker continues.

`Tool::system_prompt()` returns `ToolSystemPrompt { tools: Option<String>,
rules: Vec<String> }`. Its default is `None` and an empty list. The optional
`tools` text is rendered as `- <tool name>: <directions>` inside `<tools>`; rules
become Markdown list items in `<rules>`. Tool order and rule order are preserved;
empty entries are ignored and identical trimmed rules keep their first occurrence.
Markdown is retained and multiline list items are indented. Both sections are
saved as separate `MessageType::System` events after the tool schemas.

Hooks implement `Hook` and return `Result<(), HookError>`. Tools implement `Tool`
and return `ToolOutput { content, error }`. Both have synchronous, IO-free
`new(&Agent, &Session)` constructors. `HookKind` and `ToolKind` are runtime
instances, distinct from wire `HookId` and `ToolId`; a worker constructs its own
instances from the pinned definition when loaded. Instances live for that worker,
not globally or durably, and are reconstructed on reload. Hook invocation uses
`&mut self`, allowing agent-runtime hook state across worker generations. The output flag is model-facing,
not an agent control signal. The worker persists each tool response separately. To interrupt,
a tool/hook explicitly saves an `Interrupted` status event. Both interfaces retain
cooperative cancellation and bounded cleanup. Session settings are invocation
snapshots; mailbox updates are applied after the invocation.

The worker resolves a fresh probe handle before each inference iteration and
interrupts without inference if the probe is missing. A mid-iteration disconnect
is reported through probe-operation errors; tools/hooks decide whether to change
status. Operations are never automatically retried.

`agents::sessions::Sessions` is the shared async facade for definition create/
query/version updates, session create/query/update/fork, agent create/query,
request submission, steering, resume, interrupt, and busy queries. `Runtime::handle` and `AgentContext::sessions()` expose it.
Definitions use `psa_api::AgentDefinition` directly. Session/agent operations
return server-native `Session`, `Agent`, and `ForkedSession` values, including
internal event/position metadata; HTTP handlers convert these to wire types and
query busy status separately. Request queues and worker lifecycles
remain owned by the session-manager actor. The event-replayed transcript type is
now `agents::agent_projection::AgentProjection`, distinct from execution context.

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
        agent_projection::AgentProjection,
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
    model_config: ModelConfig {
        model: Model::new("example-model")?,
        provider: ProviderType::Openrouter,
        effort: Effort::High,
    },
}).await?;
let definition = cursor.create_agent_definition(psa_api::CreateAgentDefinition {
    name: "Manual replay example".into(),
    ..Default::default()
}).await?;
let agent = cursor.create_agent(session.id, NewAgent {
    interactive: false,
    definition_id: definition.id,
    name: "Main".into(),
}).await?;
// Direct cursor operations do not execute lifecycle hooks; seed events explicitly.

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
let mut state = AgentProjection::default();
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

`AgentProjection` is a pure reducer. `AgentHistory.items` is flat context: user prompts,
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
registered `probe_id`; forks inherit it. Workers resolve the session's probe
before inference and expose it to tools/hooks through `AgentContext::probe()`.
