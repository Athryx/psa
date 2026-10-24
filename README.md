# PSA

Work-in-progress Rust agent harness with a central server and remote Linux probes.
Clients manage sessions and agents through an async Rust client or HTTP/SSE API;
probes connect outward over authenticated WebSockets to provide file and process
operations across machines or sandboxes.

Agent creation accepts an `interactive` boolean (JSON default: `false`). It is
metadata available to tools and hooks, not a runtime behavior switch; agent
responses expose it and forks inherit it.

**Current limitations:** the client HTTP API has no authentication layer. Keep it
on loopback or behind a trusted authenticated transport. Probe operations are
available to server-side Rust integrations, but agent tools do not use them yet.
Probes execute with their process's OS permissions, not an additional sandbox.

## Workspace

| Crate | Purpose |
| --- | --- |
| `psa_server` | HTTP server, SQLite storage, and agent runtime |
| `psa_api` | API/protocol types, routes, and async client |
| `psa_shared` | Shared probe transport and local configuration locking |
| `psa_probe` | Linux remote filesystem and process execution |
| `llm_api` | Low-level model provider adapters |
| `actors`, `actors_macros` | Async mailboxes and typed client generation |

## Package just the probe with Docker

No host Rust, Zig, or native build dependencies are needed for probe packaging:

```sh
scripts/package-probe-docker.sh          # amd64
scripts/package-probe-docker.sh arm64    # or: all
```

Copy `target/probe-package/<architecture>/psa_probe` to the target machine.
An optional second argument selects another output root. This exports a
standalone binary, not a runtime container;
see the [probe README](psa_probe/README.md) for the process and multi-architecture
builds. Native workspace builds below still require prepared tool assets.

## Prerequisites

Use a Linux development host with Rust/rustup, a native C build toolchain, Git,
Python 3 with venv support, and binutils (`readelf`). Run commands below from the
workspace root.

```sh
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
rustup default stable

# Pinned probe build tools, matching the repository's CI configuration.
python3 -m venv target/build-venv
. target/build-venv/bin/activate
pip install ziglang==0.16.0
cargo install cargo-zigbuild --version 0.23.4 --locked
```

Keep the virtual environment active when running the probe packaging script.
The first build needs network access for Cargo dependencies and pinned tool
sources. SQLx uses checked-in query metadata; no running database or SQLx CLI is
needed for ordinary builds.

## Build everything

The probe embeds static `rg` and `fd` binaries plus their license files. Prepare
these assets **before** building or testing the workspace:

```sh
# x86_64 host; use aarch64-unknown-linux-musl on an ARM64 host.
PSA_PREPARE_ONLY=1 scripts/package-probe.sh x86_64-unknown-linux-musl

SQLX_OFFLINE=true cargo build --locked --workspace
```

Development executables are `target/debug/psa_server` and
`target/debug/psa_probe`. For optimized host builds:

```sh
SQLX_OFFLINE=true cargo build --locked --workspace --release
```

For standalone, statically linked Linux probes, package either architecture:

```sh
scripts/package-probe.sh x86_64-unknown-linux-musl
scripts/package-probe.sh aarch64-unknown-linux-musl
```

Outputs are `target/<target>/release/psa_probe`. The script installs the Rust
cross-compilation target, builds pinned ripgrep/fd sources, embeds them, and
checks that the resulting executables have no dynamic dependencies. Copy the
probe binary to the target machine; a Rust toolchain is not needed there.

Prepared assets live in `target/probe-assets/<architecture>`. To use another
prepared directory, set `PSA_PROBE_ASSETS` to an absolute path containing the same
executables and license files. Prepare assets for each architecture you build.

## Local quick start

Use separate server and probe data directories, even on the same machine.

### 1. Start the server

```sh
./target/debug/psa_server start \
  --data-dir ./target/psa-server-dev \
  --listen 127.0.0.1:8080
```

First startup creates these files in the selected data directory:

- `config.toml`: listen address and database path.
- `auth.toml`: generated `[psa].probe_key`, plus optional provider credentials.
- `psa.sqlite3`: database, with migrations applied automatically.

Leave the server running. In another terminal:

```sh
curl --fail http://127.0.0.1:8080/health
```

### 2. Configure and start a probe

```sh
mkdir -p ./target/psa-probe-dev
chmod 700 ./target/psa-probe-dev
```

Create `target/psa-probe-dev/auth.toml`, copying **only** the `[psa]` section from
`target/psa-server-dev/auth.toml`:

```toml
[psa]
probe_key = "REPLACE_WITH_THE_SERVER_GENERATED_KEY"
```

Do not copy model provider credentials onto probes.

```sh
chmod 600 ./target/psa-probe-dev/auth.toml
./target/debug/psa_probe start \
  --data-dir ./target/psa-probe-dev \
  --server http://127.0.0.1:8080 \
  --label local-dev
```

First startup creates `probe.toml` with a persistent UUID and the supplied server
URL/label. Later starts can omit `--server` and `--label`:

```sh
./target/debug/psa_probe start --data-dir ./target/psa-probe-dev
```

Verify registration from another terminal:

```sh
curl --fail http://127.0.0.1:8080/probes
```

The probe should have `is_active: true`. Its UUID is the `probe_id` required when
creating a session. Stop either process with Ctrl-C or SIGTERM.

## Configuration reference

Without `--data-dir`, both programs use the platform's local application-data
directory, normally `~/.local/share/psa` on Linux (honoring `XDG_DATA_HOME`).

### Server

`config.toml`:

```toml
[server]
listen = "127.0.0.1:8080"

[database]
path = "psa.sqlite3"

[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"

# Optional fallbacks for omitted component fields in API/TOML agent configs.
[agent_defaults]
bundles = ["FileTools", "Bash"]
tools = []

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Cwd", "Skills"]
after_request = ["StopWithoutTools"]
```

`[session_defaults]` supplies the model for session creation when `model_config`
is omitted or null. Config files use a `provider/api-name` string, not a structured
model object (for example `openrouter/vendor/model`). Omitting the entire section
uses OpenAI Codex `gpt-5` with `Medium` effort; when present, both fields are
required. An explicit API model override wins. The resolved concrete model is
persisted and returned in session responses; reloads, updates, and forks do not
reset it to these defaults. Restart the server to change creation defaults.

Omitted tools/bundles inherit the entire server default list; explicit lists
replace it, and `[]` disables it. Hook stages inherit independently. Without
`[agent_defaults]`, all fallback lists are empty. Defaults resolve during API
registration/file refresh, not additively or against existing snapshots. Restart
the server after changing defaults; API callers must resubmit omission-aware
input to use them. Library integrations configure defaults with
`ConfigRegistry::new(data_dir).with_agent_defaults(defaults)` passed to
`server::serve_with_configuration`; its `default_model_config: ModelConfig`
argument precedes `shutdown`. `ResolvedConfig.session_defaults` contains the
resolved model configuration.

Relative database paths resolve against `--data-dir`. `--listen` overrides the
configured address. `--config /path/config.toml` selects an existing file;
explicit missing configs are errors. Without `--config`, a missing default config
is created. Credentials always come from `auth.toml` **beside the selected config**.

To enable inference, add provider credentials to the server's existing
`auth.toml`, preserving `[psa]`, then restart the server:

```toml
[openrouter]
api_key = "YOUR_API_KEY"

# Optional adapter with compatibility limitations:
# [openai_codex]
# access_token = "YOUR_ACCESS_TOKEN"
```

Keep this file owner-only (`chmod 600`). Health, storage, and discovery do not
require provider credentials. Credential reload, login, and token refresh are
not implemented.

### Probe

Generated `probe.toml`:

```toml
id = "REPLACE_WITH_THE_GENERATED_UUID"
server_url = "http://127.0.0.1:8080"
label = "local-dev"
```

`--config /path/probe.toml` selects an existing config; its `auth.toml` must be
in the same directory. `--server` and `--label` update and persist config values.
The probe never generates its authentication key.

For a remote probe, use a reachable server address rather than `127.0.0.1`.
HTTP/WS and HTTPS/WSS URLs are supported; use TLS or a trusted encrypted tunnel
across untrusted networks. The server itself listens on plain HTTP, so HTTPS
requires a TLS-terminating proxy. Preserve WebSocket upgrades and the
`Authorization` header. Do not expose the unauthenticated client API publicly.

Each installation needs its own UUID: copying `probe.toml` copies the probe's
identity, and duplicate active UUIDs are rejected. Bundled tools are extracted to
`<data-dir>/bin` and prepended to remote commands' PATH. Probe reconnects do not
resume or replay operations; a disconnected mutation may have an unknown outcome.

## Clients and further documentation

Use `psa_api::Client` for agent configs, skills, immutable snapshots, sessions,
agents, probe discovery, and JSON/SSE event queries. Every session requires a
registered probe ID; its working directory is on that probe. Optional
`CreateSession.model_config` overrides server `[session_defaults]`; omission or
null uses the server default, persisted as a concrete session model configuration.
Batch-upsert `AgentConfigInput` values with `upsert_agent_configs`, then create an agent
with `NewConfiguredAgent { config_id, name, subagent_parent_id, interactive, model }`.
`config_id` is a `(config_group, name)` identity, not an immutable definition ID.
Creation resolves the config's allowed model/effort policies and pins an immutable
snapshot; later config upserts do not change existing agents. Optional `model`
is a `ModelSelection` that must exactly match an allowed policy, including the
fixed model and effort policy. JSON uses `"Global"`, `"Parent"`, or a `Fixed`
object. Omitted/null selects the sole policy, or the first policy when `default`
is true. Registration normalizes `default` to true for a single allowed policy;
multiple policies with `default = false` require an explicit selection.
Use `AgentConfigInput::new(config_id, models)` to inherit component defaults;
normalized `AgentConfig` values are also accepted but send all component lists
explicitly, including empty lists.

The HTTP API uses `/agent-configs` (batch POST/list GET),
`POST /agent-configs/lookup`, `/skills` (upload POST/list GET), and
`GET /agent-snapshots/{definition_id}`. Legacy `/agent-definitions` CRUD is removed,
not retained as a compatibility API. Global discovery reads
`<server-data-dir>/{agents,skills}`; project discovery selects the nearest
probe-local `.agents/{agents,skills}`. See
[configuration types, JSON/TOML examples, and snapshot semantics](docs/AGENT_DEF_SCHEMA.md).

Hook bindings inherit `[agent_defaults.hooks]` per omitted stage; explicit `[]`
disables a stage, and without configured defaults all are empty. For ordinary
execution, configure defaults or explicitly bind
`[DefaultStartup, ToolDefinitions, Cwd, Skills]` to `agent_create` in that order,
`SessionModelUpdate` to `session_update`, and `StopWithoutTools` to `after_request`;
see the server client example below. `DefaultStartup` initializes root context,
model, and base assistant identity; `ToolDefinitions` separately snapshots tools.
Both skip forks to preserve inherited context. `Cwd` emits the XML-escaped session
working directory, including on forks, and can also bind to `session_update`.
`Skills` reads the snapshot's resolved probe-local skill paths and emits an
XML-escaped catalogue of names, descriptions, and paths, not skill bodies.
Server-owned skills are copied to managed probe caches before agent creation;
probe-owned skills remain in place. Mutable `SkillId` references are stored
separately from executable skill paths. Skill paths are consumed only when
`SkillsHook` is configured. `selectable` and `extra_system_prompt` do not
automatically select agents or inject prompts; the opt-in Subagents tool enforces
the snapshot's `permitted_subagents` IDs when spawning children. There is no
remote-operation HTTP API yet; use `psa_server::probes::ProbeRegistry` handles
inside server-side integrations.

- [Server setup, client examples, storage, and SQLx workflow](psa_server/README.md)
- [Probe operations, archive handling, and connection lifecycle](psa_probe/README.md)
- [HTTP API overview](psa_api/API_OVERVIEW.md)
- [Actor mailbox API](actors/README.md)
- [Architecture](ARCHITECTURE.md)

## Development checks

Prepare the host-architecture probe assets first, as described above, then run:

```sh
cargo fmt --all -- --check
SQLX_OFFLINE=true cargo test --locked --workspace
SQLX_OFFLINE=true cargo clippy --locked --workspace --all-targets -- -D warnings
```

Tests use temporary databases and loopback listeners, not provider credentials or
your default data directory. Set `RUST_LOG=psa_server=debug,psa_probe=debug` for more
runtime logging. See the server README for regenerating `.sqlx/` metadata after
schema or query changes; always use a disposable database for that workflow.

Model discovery is available through `/providers` and `/models`, including
persistent pins, authenticated-provider filters, and four-hour Pi catalog sync.
See [the model API](psa_api/API_OVERVIEW.md#providers-and-models).
