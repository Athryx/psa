# PSA desktop

Desktop shell for the shared Dioxus GUI. Connect to an existing server using its
URL and `[psa].client_key`, or start a managed local server from the GUI. The
server and probe remain separate processes. Never supply a provider API key here.

## Run

From the workspace root:

```sh
cargo run --locked -p psa_desktop --features native
# Use a different PSA data directory:
cargo run --locked -p psa_desktop --features native -- --data-dir /custom/psa
# Select the server executable for managed local startup:
cargo run --locked -p psa_desktop --features native -- --server-binary /path/to/psa_server
```

The `native` feature enables the desktop WebView renderer. It is optional so
workspace tests, configuration services, and GUI/state tests can run without a
windowing system or WebView development packages. There is no web/mobile/TUI
executable yet.

On Debian/Ubuntu, native builds need a C toolchain, pkg-config, and WebKitGTK 4.1
development packages (including GTK 3):

```sh
sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libxdo-dev libssl-dev
```

On NixOS, enter the workspace's development shell before building **or running**:

```sh
nix-shell
cargo run --locked -p psa_desktop --features native
# Or, from outside the shell:
nix-shell --run 'cargo run --locked -p psa_desktop --features native'
```

`shell.nix` supplies native build dependencies, runtime `LD_LIBRARY_PATH`
(including `libxdo` and OpenSSL), and GTK settings schemas. It preserves existing
library/data paths and uses your installed Rust/rustup toolchain. The shell uses
`<nixpkgs>` from your Nix environment; it does not pin a separate package set.

Native builds require substantially more disk space than the headless crates.
The executable needs a graphical session to display its window.

## Settings

The desktop reads/writes `<psa-data-dir>/desktop.toml`, using the same PSA data
directory convention as the server and probe. On Linux the default is
`~/.local/share/psa/desktop.toml`. Missing files use defaults; malformed existing
files are reported, not silently replaced. Configure connection profiles in the
GUI; legacy settings with a single `[connection]` are migrated when loaded.

Settings are saved asynchronously through a debounced, latest-snapshot writer,
using a private temporary file and atomic replacement. Window close waits for the
latest durable state to flush; on failure the window reopens so the save can be
retried. Forced process termination cannot guarantee a final flush. On Unix the resulting file is
mode 0600. The client key is stored in plaintext: protect this directory, and do
not commit or share the file. Parse errors and Debug output do not include the
key. Desktop settings do not overwrite `auth.toml`, `config.toml`, or `probe.toml`.

Tab/split layout and selection are scoped to connection profiles and restored on restart.
Unsent drafts are not persisted. Favorites are the connected server's model pins,
shared with its other clients, not a local desktop favorites list.

## Architecture

- `client_shared`: transcript/session projections, grouping, workspace reducers,
  and serializable settings. No Dioxus dependency.
- `psa_gui`: dark shared UI, async HTTP/SSE orchestration, settings surface, and
  object-safe `GuiPlatform` interface. No native renderer dependency.
- `psa_desktop`: platform implementation, data-directory resolution, settings
  persistence, CLI, window configuration, and renderer launch.

The `/model` command changes the focused tab's **session default**. It does not
override agents whose definitions pin their own model.

## Checks

```sh
cargo test --locked -p client_shared -p psa_gui -p psa_desktop
node --test client/psa_gui/src/*.test.cjs
cargo check --locked -p psa_desktop --features native
```

The second command needs the platform dependencies above; the first does not.
