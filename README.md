# PSA

Work-in-progress Rust agent harness with a central server and remote Linux probes.
Clients manage sessions and agents through an async Rust client or HTTP/SSE API;
probes connect outward over authenticated WebSockets to provide file and process
operations across machines or sandboxes.

**Current limitations:** the client HTTP API has no authentication layer. Keep it
on loopback or behind a trusted authenticated transport. Probe operations are
available to server-side Rust integrations, but agent tools do not use them yet.
Probes execute with their process's OS permissions, not an additional sandbox.

## Workspace

| Crate | Purpose |
| --- | --- |
| `psa_server` | HTTP server, SQLite storage, and agent runtime |
| `psa_api` | Shared types, routes, async client, and probe transport |
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
```

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

Use `psa_api::Client` for sessions, agents, probe discovery, and JSON/SSE event
queries. Every session requires a registered probe ID; its working directory is
a path on that probe. There is no remote-operation HTTP API yet; use
`psa_server::probes::ProbeRegistry` handles inside server-side integrations.

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
