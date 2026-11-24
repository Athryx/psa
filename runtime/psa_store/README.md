# PSA store

Durable SQLite storage for the PSA server. SQLite, row types, and the pool are
private to this crate; callers exchange domain sessions, agents, events,
configurations, and catalog records through cursors. It does not depend on the
agent runtime: bundle configuration is validated by the server before it is
written, and event replay into model context lives in the server's
`AgentProjection`.

Enable the `test-support` feature in another crate's dev-dependencies for
shared fixtures (`test_support::session_fixture`, `register_probe`, ...).

## Cursors

`Database` manages the pool and migrations. Call `db.read().await?` for a
`ReadCursor` or `db.write().await?` for a `WriteCursor`; all session, agent, and
event operations take `&mut self` on a cursor. Queries live on `ReadCursor`, and
`WriteCursor` adds the writes and dereferences to `ReadCursor`, so it can read
too. On a write cursor, call `commit().await?` to persist changes or
`rollback().await?` to discard them; dropping it also rolls back. A read cursor
ends with `close().await?` or on drop.

A read cursor begins a deferred transaction: under WAL it reads a consistent
snapshot without taking the writer lock, so reads never wait on writers. A write
cursor begins an immediate transaction, reserving the writer lock up front so a
read-then-write can never fail to upgrade. Keep cursors short lived (an open read
transaction also stalls WAL checkpoints) and release them before
model/tool/network work or closing the database.
Multiple operations share one coherent snapshot and can be committed atomically.
Multi-step writes use savepoints: a failed operation rolls back its own changes,
so callers can catch the error and still commit earlier successful operations.
SQL row types and the pool are private. All new errors use `thiserror`.
Each `Agent` exposes its current `state`. New agents start idle; event appends
atomically update this field using the last `StatusChange` in the batch. A fork
inherits the state visible at its cutoff, not its parent's later state. Migration
0005 backfills existing agents from their bounded event histories.
`add_events` returns `AddEventResult` with the assigned `events` and
`new_state: Option<AgentState>` and `new_usage: Option<Usage>`. These are
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

## SQLx schema and offline builds

The migration directory is the authoritative schema. Normal builds use the
checked-in workspace `.sqlx/` query metadata and require no database connection.
To regenerate metadata after changing SQL or migrations, use a scratch database:

```sh
cargo install sqlx-cli --version 0.9.0 --locked --no-default-features --features sqlite
export DATABASE_URL="sqlite://$(mktemp -d)/psa-schema.sqlite3"
sqlx database create
sqlx migrate run --source runtime/psa_store/migrations
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
