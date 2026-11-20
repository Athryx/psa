# PSA probe

`psa_probe` is a Linux remote execution endpoint. It connects outward to PSA using
an authenticated WebSocket. A connection provides concurrent file operations,
process pipes and signals, and streaming tar directory transfers. Operations are
currently available to server-side Rust integrations; agent tools do not use them.

## Packaging overview

`scripts/package-probe.sh` is the packaging entry point. It:

1. Installs the selected Rust musl target and LLVM strip tool.
2. Fetches pinned ripgrep/fd sources and cross-compiles static `rg` and `fd`.
3. Stages those tools and license notices in `target/probe-assets/<architecture>`.
4. Builds `psa_probe` with the tools/licenses embedded, then strips the binary.
5. Uses `readelf` to reject dynamic dependencies in the probe and bundled tools.

The deliverable is **one executable**, not an archive or runtime container:
`target/<target>/release/psa_probe`. Copy it to the target Linux machine. At startup,
it extracts its embedded tools/licenses into the configured data directory.
Authentication and configuration are separate files, never embedded in the binary.

## Build with Docker

Only Docker with Buildx/BuildKit is needed on the host. From the workspace root:

```sh
scripts/package-probe-docker.sh          # amd64
scripts/package-probe-docker.sh arm64
scripts/package-probe-docker.sh all      # both, built separately
scripts/package-probe-docker.sh all ./dist/probes
```

Binaries are exported to `target/probe-package/<architecture>/psa_probe` by default.
The optional second argument selects another output root, relative to your current
working directory. Rust target names are also accepted. The script works from any
working directory when invoked by its path. It cross-compiles on the builder's
native architecture without QEMU; `all` uses two builds so it does not require a
multi-platform builder.

For direct Docker access (including extra build options), the equivalent command is:

```sh
docker buildx build -f runtime/psa_probe/Dockerfile \
  --platform linux/amd64 \
  --output type=local,dest=target/probe-package/amd64 .
```

To export both architectures in one Docker build:

```sh
docker buildx build -f runtime/psa_probe/Dockerfile \
  --platform linux/amd64,linux/arm64 \
  --output type=local,dest=target/probe-package .
```

Multi-platform exports place binaries under `linux_amd64/` and `linux_arm64/`.
If your default builder does not support multi-platform builds, first create one
with `docker buildx create --name psa-probe-builder --driver docker-container --use`.

The Dockerfile is a **probe-only build environment**, reusing the packaging script;
it does not build the server or run the probe in a container. It installs pinned
Zig/cargo-zigbuild versions matching CI, with cached Cargo dependencies and build
outputs. First builds need network access and several GB of disk space. The
Dockerfile-specific ignore file excludes host build outputs and auth/config files.

The base Rust image follows `rust:slim-bookworm`; to select a specific toolchain,
pass `--build-arg RUST_IMAGE=rust:<version>-slim-bookworm` (or an image digest).
`ZIG_VERSION` and `CARGO_ZIGBUILD_VERSION` are also build arguments.

## Build without Docker

Install Rust, Git, Zig, binutils (`readelf`), and `cargo-zigbuild` (`cargo install cargo-zigbuild --version
0.23.4 --locked`). From the workspace root:

```sh
scripts/package-probe.sh x86_64-unknown-linux-musl
scripts/package-probe.sh aarch64-unknown-linux-musl
```

The script builds pinned ripgrep 15.2.0 and fd 10.5.0 sources for the selected musl
target, embeds the executables and license notices, builds the probe, and checks
that all three executables have no ELF interpreter or shared-library dependencies.
The resulting probe is `target/<target>/release/psa_probe`.

Cargo does not download tool assets implicitly. Before ordinary workspace builds
or tests, run `PSA_PREPARE_ONLY=1 scripts/package-probe.sh
x86_64-unknown-linux-musl` (use aarch64 on an aarch64 development host). Prepared
assets live in `target/probe-assets/<architecture>`; `PSA_PROBE_ASSETS` can point to
an alternative directory containing the same executables and license files.

## Configure and start

Start `psa_server` once to generate `[psa].probe_key` in its `auth.toml`. Copy only
that key into the probe's `[psa]` section; do not copy the separate `client_key` or
provider credentials onto probes. The default data directory on Linux is
`~/.local/share/psa`.

```toml
# auth.toml
[psa]
probe_key = "copy the server's generated key here"
```

```sh
psa_probe start --server http://127.0.0.1:8080 --label workstation
# Optional: --data-dir /custom/psa --config /custom/probe.toml
```

The first start creates `probe.toml`, generates a UUID, and persists the supplied
server URL and label. Subsequent starts can omit these options. Explicit missing
config files and malformed config files are errors. `auth.toml` is resolved beside
the selected config file; it is never generated by the probe.

The server can start a local probe in its own process by setting
`[server] autostart_local_probe = true` in its configuration (default: false).
It uses the server data directory for `probe.toml` and bundled tools, and the
server configuration and adjacent `auth.toml` for the host marker and key.
The actual listener address is persisted as `server_url` in `probe.toml` on
startup; wildcard addresses use loopback. The probe stops with the server, after
session cleanup. It runs with the server's filesystem and process permissions.

For the host probe, use the server's data/config directory. The server's generated
`config.toml` contains root `host = true`; the probe reads that file beside its
selected `probe.toml` and sends the flag with its initial connection. Missing
`config.toml`, a missing marker, or `host = false` means a remote probe. Existing
server configs are not rewritten; add the root marker manually to opt in.
Unrelated server settings are ignored by the probe, but malformed TOML or an
invalid host value fails startup.

The flag is persisted and exposed as `host` in probe discovery. At most one
registered probe may be host. Host probes use original server-owned skill paths
without syncing or modifying a skill cache; remote probes retain managed skill
synchronization. Only enable this marker for a shared filesystem, not merely the
same network address. Do not copy the host marker to remote probes.

```toml
# probe.toml, created automatically
id = "<persistent UUID>"
server_url = "https://psa.example"
label = "workstation" # optional
```

The probe extracts its bundled static tools into `<data-dir>/bin`, refreshes them
when their embedded hashes change, and writes license notices into `licenses`.
Remote commands have this bin directory prepended to PATH, including when their
environment is cleared. Use WSS/HTTPS server URLs for TLS connections; plain
WS/HTTP URLs are also supported. File/process operations run with the probe
user's normal OS permissions, across its filesystem.

A UUID identifies this data directory, rather than immutable hardware. Copying a
probe config to another machine copies its identity; duplicate active UUIDs are
rejected. Regenerate the UUID when deliberately creating another installation.

## Sandbox images

A probe with `supports_sandboxes = true` creates Docker sandboxes that run a
nested probe as the container's entrypoint, so the image's own `ENTRYPOINT` and
`CMD` do not run. Images can declare startup and shutdown work instead:

```dockerfile
ENV PSA_SETUP="service postgresql start"
ENV PSA_TEARDOWN="service postgresql stop"
ENV PSA_TEARDOWN_TIMEOUT=30
```

- `PSA_SETUP` runs with `/bin/sh -c` on every container start, before the probe
  connects. Make it idempotent. Background daemons it starts keep running; only
  the shell is awaited. A failure is logged and the probe connects anyway.
- `PSA_TEARDOWN` runs when the container stops (including a plain `docker stop`).
  It is best effort: a killed container, crash, or host reboot skips it.
- `PSA_TEARDOWN_TIMEOUT` is in whole seconds (default 10, maximum 300). The
  manager reads it from the image when creating the sandbox and sets Docker's
  stop timeout a few seconds longer; recreate the sandbox to apply a new value.

- `PSA_SANDBOX_NAME` is set on every sandbox container to the sandbox's name,
  so hooks (and agent commands) can tell sandboxes apart.

Hook output is appended to `/opt/psa/data/image-hooks.log` in the container. The
`PSA_*` hook variables are removed from the environment of commands agents run.

## Server-side operations

Retain a `psa_server::probes::ProbeRegistry` and pass it to
`serve_with_probe_registry`. Obtain a connection-specific handle with
`registry.handle(probe_id).await`. The handle exposes `read_file`, `write_file`,
`list_dir`, `create_dir`, `upload_dir`, `sync_dir`, `download_dir`, and `spawn`.

File and archive methods return a `Call`. Its `sender.write(0, bytes)` sends input,
`sender.close_stdin()` ends input, and `next_event()` yields binary output chunks,
EOF, completion, or an error. Consume output concurrently with writing large
process input. Per-stream backpressure bounds memory; consumers must drain both
stdout and stderr through `next_event()`.

```rust,ignore
let handle = registry.handle(probe_id).await?;
let mut child = handle.spawn(
    psa::api::probes::Command::new("rg", "/work").arg("TODO")
).await?;
loop {
    let event = child.next_event().await?;
    match event {
        psa::api::probes::Event::Data { stream: 1, bytes } => { /* stdout */ }
        psa::api::probes::Event::Data { stream: 2, bytes } => { /* stderr */ }
        psa::api::probes::Event::Complete { status } => break,
        _ => {}
    }
}
```

Commands support args, cwd, environment overrides/removal, environment clearing,
and piped/null/inherited stdio (inherited means the probe's own streams). Signal
with `signal(unix_signal_number)` or `kill()`. Use `wait()` to drain output and
return exit status, or `into_output()` to collect stdout and stderr in memory. No PTY is provided. Dropping a live call
requests cancellation. Process groups are killed and direct children reaped on
cancellation, disconnect, or shutdown, including descendants left behind by an
exited group leader. Programs deliberately escaping their group are outside this
lifecycle guarantee.

Filesystem operation paths must be absolute UTF-8 paths. Directory transfers
stream uncompressed tar archives. Supported entries are
regular files and directories, preserving permissions; links, special files, and
escaping paths are rejected. Upload targets must not exist and their parent must
exist. Uploads are staged beside the target and published with a non-replacing
rename after stream EOF. `sync_dir` (wire operation `sync_dir`, with `path`) uses
that same streaming tar input and validation, but replaces the entire destination
tree, removing stale files. Its parent must exist. Missing targets are created
with a non-replacing rename; existing directories are atomically exchanged using
Linux `renameat2(RENAME_EXCHANGE)`. Files and symlink targets are rejected. Staging
is on the same filesystem, beside the target; there is no delete-then-rename gap
or non-atomic fallback. Unsupported exchange operations fail without replacing
the target. After exchange, the old tree is removed before completion (including
read-only directories, without following symlinks). Probe archive publications
are serialized per destination (including parent path aliases); archive reception
and extraction remain concurrent. Mutations by external processes, and renames
of a destination's parent, are not coordinated. Failed or cancelled transfers before publication leave the
old destination intact and remove staging data. Cancellation/disconnection at
publication can leave the new tree installed; a cleanup failure after publication
is reported as an error but does not roll back the swap. Ordinary file writes are
incremental: failure can leave a partial file.

Probe protocol version 2 includes `sync_dir`; version 1 probes are rejected at
connection time. Upgrade the server and probes together.

Connections heartbeat every 10 seconds and expire after 30 seconds without a
pong. After cleanup, the probe retries network failures and duplicate UUID
responses with jittered 1–30 second backoff. Credential and protocol rejection is
fatal. Operations are never replayed or reattached after reconnect; obtain a new
handle, and treat a disconnected mutation's outcome as unknown.

## Discovery and sessions

`GET /probes` and `GET /probes/{uuid}` return persisted metadata, creation and
last-connection timestamps, and live `is_active`. When the probe process's
`HOME` is an absolute UTF-8 path of at most 4096 bytes, metadata also includes
`home_dir` as a directory browsing hint; it may be absent or inaccessible.
Offline records remain visible. These discovery routes retain the server's trusted-client policy; the shared
probe key authenticates only `GET /probes/connect`.

Every session requires a registered `probe_id`. Offline probes are accepted;
association is fixed and inherited on fork. Working-directory paths belong to
the probe and are not checked on the server. This development schema change does
not migrate existing databases.
