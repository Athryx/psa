# PSA

A work-in-progress Rust agent harness with a central server, clients, and remote Linux probes.

## Build

On Linux, install Rust/rustup, a C toolchain, Git, Python 3 with venv support, and binutils (`readelf`). From the workspace root:

```sh
rustup toolchain install stable --profile minimal --component rustfmt --component clippy

# Install zig for corss compiling fd and rg for different platforms
python3 -m venv target/build-venv
. target/build-venv/bin/activate
pip install ziglang==0.16.0
cargo install cargo-zigbuild --version 0.23.4 --locked

# Prepare the probe's embedded tools; use aarch64-unknown-linux-musl on ARM64.
PSA_PREPARE_ONLY=1 scripts/package-probe.sh x86_64-unknown-linux-musl
SQLX_OFFLINE=true cargo build --locked --workspace
```

The first build needs network access. For a standalone probe, run `scripts/package-probe-docker.sh` (or pass `arm64` or `all`); see the [probe README](runtime/psa_probe/README.md).

## Run locally

Start the server, which creates its configuration, authentication keys, and database on first run:

```sh
./target/debug/psa_server start --data-dir ./target/psa-server-dev --listen 127.0.0.1:8080
```

In another terminal, start a host probe using the same data directory:

```sh
./target/debug/psa_probe start --data-dir ./target/psa-server-dev \
  --server http://127.0.0.1:8080 --label host
```

For a separate machine, create a probe data directory with an `auth.toml` containing only the server's `[psa].probe_key`, then start the probe with that directory and a reachable server URL. See the [probe README](runtime/psa_probe/README.md). Use TLS or a trusted encrypted tunnel across untrusted networks.

To run the desktop client, install its [native dependencies](client/psa_desktop/README.md), then:

```sh
cargo run --locked -p psa_desktop --features native -- --server-binary ./target/debug/psa_server
```

## Configuration

The server creates `config.toml` and `auth.toml` in its data directory (normally `~/.local/share/psa` on Linux). Set the model in `[session_defaults]` and add provider credentials to `auth.toml`, preserving the generated `[psa]` keys:

```toml
# config.toml
[session_defaults]
model = "openrouter/vendor/model"
effort = "Medium"
```

```toml
# auth.toml
[openrouter]
api_key = "YOUR_API_KEY"
```

Keep `auth.toml` private and restart the server after configuration changes. See the [server README](runtime/psa_server/README.md) for configuration, CodeMode, and client examples, and [agent configuration](docs/AGENT_DEF_SCHEMA.md) for bundle and hook formats.

## Layout

- `runtime/`: server, probe, and shared runtime utilities.
- `client/`: desktop application, GUI, and shared client state.
- `lib/psa/`: API types and async client library.
- `lib/`: LLM adapters and actor libraries.
- `scripts/`: build and packaging scripts; `docs/`: additional documentation.

## Checks

After preparing probe assets as above:

```sh
cargo fmt --all -- --check
SQLX_OFFLINE=true cargo test --locked --workspace
```
