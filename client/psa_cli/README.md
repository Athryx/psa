# psa

The `psa` command is PSA's terminal client. Run without a subcommand, it opens
a full-screen TUI. Its subcommands run and manage local PSA processes; the
server, desktop client, and probe remain separate executables that `psa`
finds and launches.

## Terminal client

```sh
psa                                   # local server of the data directory, started if needed
psa --url http://host:8080 --key KEY  # another server; or set PSA_URL and PSA_KEY
```

The TUI shows one agent's transcript with an input line below it. It starts in
a new session in the current directory on this machine's probe; `/sessions`
opens an existing one instead. A new session exists only in the TUI until its
first message, which creates the session, its root agent, and then sends the
message. Before that, `/rename` names it (otherwise the first message does,
cut to about 48 characters), `/select agent` chooses the root agent and its model
policy from the global configurations and the directory's own `.agents`,
`/model` and `/effort` choose the session model, and `/select probe` moves it to
another connected probe, in that probe's home directory.

Type `/` for the command menu: Tab completes a command, Enter runs it.
Commands with choices list them as you type their arguments, in the same menu
as the desktop client: `/model` (pinned models and the current one, with a
"Show all models" item at the end), `/effort` (the current model's efforts),
`/select agent`, `/select probe`, `/goal`, `/sandbox` and `/server`. Typing the choice, as in
`/effort high`, works too.

| Command | |
| --- | --- |
| `/sessions`, `/new` | Open a recent session, or start a new one in the open session's probe and directory |
| `/select agent`, `/select probe` | Before the first message: the root agent, or the probe to start on |
| `/agents`, `/info` | The session's details and its agents to switch to, or the agent's details and token usage, broken down as in the desktop client |
| `/model`, `/effort`, `/rename`, `/fork`, `/refresh`, `/compact`, `/goal`, `/sandbox` | As in the desktop client; `/rename` without a name opens an input with the current one |
| `/settings`, `/help` | Settings; `/help` opens their commands and keys tab |
| `/pinned-models`, `/pinned-decision-models` | Every model of that kind; Enter pins or unpins the highlighted one. Pins are stored on the server. |
| `/probes`, `/server status\|restart\|stop` | Probes and sandboxes; manage the local server |
| `/quit` | |

Below the input, the first line shows the working directory, then the model,
its effort (colored as in the desktop) and context usage. The second shows the
session's name and its agents, each with a dot that is green while it runs and
yellow when interrupted. Alt+[ and Alt+] open the previous and next agent, and
the list scrolls, with ‹ and › marking agents past either end. Terminals that
send Alt+[ as an escape sequence (without the kitty keyboard protocol, as in
tmux by default) can use Alt+, and Alt+. instead.

While the agent works, the input's › becomes a spinner. Ctrl+J inserts a new line. Esc interrupts a running agent, Ctrl+O shows everything (thinking, harness and
system messages, logs, statuses, full tool output) regardless of settings, and
PgUp/PgDn or the mouse wheel scroll; clicking or dragging on the scrollbar at
the right edge jumps there. Mouse capture means selecting text needs your
terminal's bypass modifier (usually Shift). When an agent asks a question, a dialog
opens; Esc leaves it for later and Ctrl+Q reopens it. Commands come from the
registry in `client_shared::commands`, shared with the desktop client. It says
what each command's arguments are, where it can run, and whether it is saved in
the agent's transcript: commands that change the agent or its session are, ones
that only show something are not. `/select probe` is the one TUI-only command.

### Settings

`/settings` changes take effect at once and are saved to `tui.toml` in the
data directory when the dialog closes. Settings the desktop client also has use
its names and defaults: reasoning (hidden, first line, shown), turn dividers
(hidden, line, line with turn number and usage), harness logs, harness
messages, text recorded results, agent statuses, and internal agents. The TUI
adds a theme (dark, light, or monochrome), system prompts (hidden, a one-line
label, or in full; a label by default, like the desktop's collapsed prompt),
tool output lines, and a maximum transcript width. The file is separate from `desktop.toml`, since each client
rewrites its whole file and would otherwise overwrite the other's changes. A
malformed `tui.toml` stops the TUI with an error rather than being replaced.

```toml
[preferences]
theme = "Light"
reasoning_display = "Collapsed"   # Hidden, Collapsed, Shown
turn_dividers = "Labeled"         # Hidden, Line, Labeled
tool_output_lines = 3             # 0 hides output; errors still show
max_width = 0                     # 0 uses the terminal width
```

## Managing processes

```sh
psa server start      # start in the background; logs go to <data-dir>/logs/server.log
psa server status     # exit status 0 when running, 3 when stopped
psa server restart
psa server stop
psa server run        # foreground, logs on the terminal; args after -- go to psa_server start
psa gui               # launch the desktop client for the same data directory
psa probe run --server http://host:8080 --label laptop   # foreground probe
```

`--data-dir` (or `PSA_DATA_DIR`) selects the data directory; the default is the
platform data directory (`~/.local/share/psa` on Linux). The server is managed
through the private control socket in that directory, so the GUI and the CLI see
the same server whichever of them started it.

Every launcher, including the desktop client and the TUI, starts the server on
the `listen` address in `config.toml`; `server start` and `restart` accept
`--listen` to override it. If that port is taken, startup fails and the error
quotes the end of the server log.

## Finding executables

Each launched executable is resolved in this order:

1. `--server-binary`, `--desktop-binary`, or `--probe-binary`
2. `PSA_SERVER_BIN`, `PSA_DESKTOP_BIN`, or `PSA_PROBE_BIN`
3. The executable beside `psa` (an install directory, or `target/debug` in a
   workspace build)
4. `PATH`

`psa gui` passes the server it found to the desktop client. The server commands
pass the probe they found to the server as `--probe-binary`. The server's
built-in local probe mounts it into sandboxes when `probe.toml` enables
`supports_sandboxes` without setting `sandbox_probe_path`. Sandboxes need a
static probe (see the [probe README](../../runtime/psa_probe/README.md)). A
workspace debug build is linked dynamically, so set `sandbox_probe_path` or
`PSA_PROBE_BIN` to a packaged probe when you use sandboxes.

## Probes

`psa probe run` runs `psa_probe start` in the foreground. A local server already
runs its own probe for its data directory, so give an additional probe on the
same machine its own `--data-dir`.

The control socket path must fit the Unix socket path limit (about 100 bytes),
so very deep data directories cannot be managed.
