# PSA server

Agent components live under `src/components/`: `bundles/` owns shared resources
and assembly, `tools/` implements model-facing tools, and `hooks/` implements
lifecycle callbacks. Shared invocation cancellation lives in
`src/components/cancellation.rs`. Tools and hooks can request typed client input
through `AgentContext::query_client`; see `../../docs/CLIENT_QUERIES.md` for the
runtime-owned query registry, SSE snapshots, response endpoint, and opt-in native
`AskQuestion { interactive_only }` bundle (one `ask_question` tool, no hook). Native Goal
and Subagents bundles are documented in `../../docs/GOALS_AND_SUBAGENTS.md`.

Provider discovery and model metadata are available through `/providers` and
`/models`. The server stores Pi-derived chat catalogs in SQLite, refreshes them
in the background every four hours, and supports forced refresh, persistent pins,
and filtering by configured credentials. See
[`../../psa_api/API_OVERVIEW.md`](../../psa_api/API_OVERVIEW.md#providers-and-models)
for routes, metadata fields, and catalog-aware reasoning behavior. A fresh database
has no models until its first successful sync; no baseline catalog is bundled.

Start the server from the workspace root:

```sh
cargo run -p psa_server -- start
```

The Clap CLI supports `--data-dir PATH`, `--config PATH`, and `--listen IP:PORT`
after `start`. Use an isolated directory for development:

```sh
cargo run -p psa_server -- start --data-dir ./target/psa-dev --listen 127.0.0.1:8080
# Copy [psa].client_key from ./target/psa-dev/auth.toml.
export PSA_CLIENT_KEY="REPLACE_WITH_THE_SERVER_GENERATED_CLIENT_KEY"
curl -H "Authorization: Bearer $PSA_CLIENT_KEY" http://127.0.0.1:8080/health
```

The default directory is the platform's local application-data directory from
`directories::ProjectDirs`, normally `~/.local/share/psa` on Linux and honoring
`XDG_DATA_HOME`. Both `config.toml` and `psa.sqlite3` live there. The default config
is created only if missing; an explicit `--config` must already exist. Invalid
configuration fails startup without overwriting it.

```toml
host = true # Marks this config directory as sharing the server's filesystem.
default_agent_name = "default" # Suggested agent configuration name for clients.

[server]
listen = "127.0.0.1:8080"
autostart_local_probe = true # Start a probe on the server host.

[database]
path = "psa.sqlite3"

[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"

# Optional fallbacks for omitted API/TOML agent component fields.
[agent_defaults]
bundles = [{ FileTools = { read_only = false } }, "Bash", "Subagents", { Goal = { interactive_only = true } }]

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
before_request = [{ Compaction = {} }]
after_request = []
```

`default_agent_name` is a client suggestion, not an agent registration or creation policy.
It defaults to `"default"` when omitted, including in existing files. Authenticated
`GET /config` returns this client-safe option; use `Client::get_config()` from
`psa_api`. Changes take effect after restarting the server.

`[session_defaults]` provides the model for new sessions that omit `model_config`
or send null. Model configuration files use a provider/API-name slug string, such
as `openrouter/vendor/model`, not a structured `Model`. If the entire section is
absent, the built-in default is OpenAI Codex `gpt-5` with `Medium` effort. If the
section is present, both `model` and `effort` are required. Explicit API model
configurations override it. The concrete chosen configuration is persisted and
returned in session responses; reloads, updates, and forks do not reapply defaults.
Changing creation defaults requires a server restart.

Omitted `bundles` inherit the whole default list; explicit lists replace
it, and `[]` disables it. Hook stages inherit independently: an omitted stage
inherits, while `after_request = []` disables only that stage. No
`[agent_defaults]` means all fallbacks are empty. Defaults apply during API
registration/file refresh, not additively or to existing snapshots. Restart to
load changed server defaults; API callers must resubmit omission-aware input to
adopt them. Library servers can use
`ConfigRegistry::new(data_dir).with_agent_defaults(defaults)` with
`server::serve_with_configuration(listener, database, providers, probes,
configuration, default_model_config, client_key, shutdown)`. Pass a concrete `ModelConfig`
as `default_model_config`; CLI resolution exposes it as
`ResolvedConfig.session_defaults`.

Set `[server].autostart_local_probe = true` to opt in to starting a local
probe with the server. The setting defaults to `false` when omitted, including
in existing configurations. A local probe executes operations on the server host;
enable it only when that access is intended. Remote probes can still be started
separately; see [the probe README](../psa_probe/README.md).

Relative database paths resolve against the selected data directory. The listen
CLI option overrides TOML. Startup applies embedded migrations before listening;
Ctrl-C or SIGTERM stops accepting commands, interrupts active agents, waits for
tool/hook cleanup and persistence, joins workers, closes SSE, drains HTTP, and
closes SQLite. Queued but unconsumed messages are memory-only and discarded.
No provider credentials are needed for storage/query operations or `/health`,
but the client bearer key is required.

## HTTP client and credentials

Every client HTTP/SSE endpoint, including `/health`, requires
`Authorization: Bearer <client_key>` using `[psa].client_key`. Missing or invalid
credentials return HTTP 401. This key grants full trusted-client access, not
per-user authorization. The separate `/probes/connect` WebSocket route requires
`[psa].probe_key` instead; neither key substitutes for the other. Use TLS or a
trusted encrypted tunnel remotely, and keep both keys secret.

Place `auth.toml` beside the selected `config.toml` (including with `--config`):

```toml
[psa]
client_key = "YOUR_CLIENT_KEY"
probe_key = "YOUR_SEPARATE_PROBE_KEY"

[openrouter]
api_key = "YOUR_API_KEY"

# Optional; uses the existing llm_api adapter and its compatibility limitations.
#[openai_codex]
#access_token = "YOUR_ACCESS_TOKEN"
```

Create the file with owner-only access (`chmod 600 auth.toml` on Unix). The server
creates a missing file and adds separate `[psa].client_key` and `[psa].probe_key`
values when absent, preserving existing keys and provider credentials. Missing provider auth means no providers configured; malformed auth
fails startup without printing source excerpts/secrets. Authenticated clients can
set or remove provider credentials through `PUT /providers/credential`; credentials
are persisted with owner-only permissions and applied to subsequent inference and
catalog status without restart. In-flight requests retain their existing provider;
queued or future inference can fail if its provider is removed before it starts.
This endpoint grants every client key holder control over provider credentials and
potential spending: client keys are full-trust administrator credentials. Use a
trusted encrypted tunnel for remote access. External edits to `auth.toml` are not
automatically reloaded; restart or a subsequent successful credential update loads them. Login and token refresh are not implemented.
Codex takes a ChatGPT Codex access token, not an OpenAI platform API key; configured
status does not verify that credentials work.

`psa_api` provides shared request/response types, route definitions, and an async
`Client`. Session views include runtime-derived `busy`; agent views include
lifecycle state and cumulative usage. Requests are accepted into bounded queues,
not necessarily persisted or executed at the time HTTP 202 is returned.

```rust,no_run
use psa_api::{Client, CreateSession, NewConfiguredAgent, AgentConfigInput, AgentRequest,
    ConfigGroup, ConfigId, HookConfig, HookOverrides, IncludedItems, MessageDelivery, ModelSelection};
use llm_api::{api::{ContentBlock, TextBlock}, providers::{Effort, Model, ModelConfig, ProviderType}};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client_key = std::env::var("PSA_CLIENT_KEY")?; // Server auth.toml: [psa].client_key.
let client = Client::new("http://127.0.0.1:8080", &client_key)?;
let probe = client.list_probes().await?.into_iter().next().ok_or("connect a probe first")?;
let session = client.create_session(&CreateSession {
    probe_id: probe.metadata.id,
    name: "Example".into(), working_directory: "/work/project".into(),
    model_config: Some(ModelConfig {
        model: Model::new(ProviderType::Openrouter, "your-provider/model")?,
        effort: Effort::Medium,
    }), // Use None to accept server session_defaults.
}).await?;
let config_id = ConfigId { config_group: ConfigGroup::Custom("examples".into()), name: "Assistant".into() };
let mut config = AgentConfigInput::new(config_id.clone(), vec![ModelSelection::Global]);
config.description = "Example assistant".into();
config.selectable = true;
config.bundles = Some(vec![psa_api::BundleConfig::FileTools { read_only: false }]); // Replace defaults.
config.hooks = HookOverrides {
    agent_create: Some(vec![
        HookConfig::DefaultStartup, HookConfig::ToolDefinitions, HookConfig::Cwd, HookConfig::Skills,
    ]),
    session_update: Some(vec![HookConfig::SessionModelUpdate]),
    after_request: Some(vec![HookConfig::StopWithoutTools]),
    ..Default::default() // Omitted before_request inherits its server default.
};
config.skills = IncludedItems { groups: vec![ConfigGroup::Global], items: vec![] };
client.upsert_agent_configs(&[config]).await?;
let agent = client.create_agent(session.id, &NewConfiguredAgent {
    config_id,
    subagent_parent_id: None,
    interactive: false,
    hidden: false,
    model: None,
    name: "Main".into(),
}).await?;
client.submit_request(session.id, agent.id, &AgentRequest::Message {
    delivery: MessageDelivery::Queue,
    content: vec![ContentBlock::Text(TextBlock { text: "Hello".into(), annotations: vec![] })],
}).await?;
# Ok(())
# }
```

Routes are defined in `psa_api::routes`: batch upsert/list/lookup agent configs,
read immutable agent snapshots, upload/list skills, create/list/get/update/fork
sessions, create/list/get agents, submit requests, and query session or agent events. Event
GETs return paged JSON with `Accept: application/json` (the default) or SSE with
`Accept: text/event-stream`. There is no `stream` query field. `after` is an exclusive agent
EventId on an agent route, or shared session position on a session route;
`through` is inclusive. `limit` accepts 1–10,000. JSON pages expose `head`,
`next_after`, and `has_more`. Session lists default to roots; `parent` lists
immediate children.

`Client::events()` requests JSON; `Client::stream_events()` requests SSE.
`Last-Event-ID` is an SSE resume header used only by the streaming method, not JSON
page requests. Use `Client::stream_events(session, optional_agent, query, last_event_id)` with
`include_pending=true` to receive provisional upserts, committed events with
optional promotion IDs, and removal messages. Upserts contain complete current
snapshots with stable IDs/revisions, not independently persisted token deltas.
The initial `Snapshot` replaces pending client state after replaying the captured
durable head. Persist the stream's last SSE ID for reconnect; replayed committed
events must be upserted by viewed agent/event identity, not blindly appended.
The client intentionally leaves reconnect policy to callers: after EOF or
`ResyncRequired`, open another stream using that durable ID. Headers are supported
by using `Client::with_http_client(base_url, client_key, http)` with a configured
reqwest client. Both constructors require a client key and send it as a bearer
token for every request, including SSE; it overrides any default Authorization
header on the custom transport.

`SessionUpdated` publishes a full `Session` snapshot for both metadata edits and
runtime `busy` changes; there is no separate `SessionBusy` message. Metadata writes
and the supervisor's published activity projection share the publication gate,
preventing stale busy values from overwriting newer transitions. Apply these
snapshots in stream order: activity-only changes do not advance `updated_at`.

### CodeMode

The opt-in `CodeMode` bundle installs the model-facing `codemode` and
`reset_codemode` tools. `codemode` runs Monty 1.4's Python subset in an agent-local
REPL, not a full CPython environment.
Monty supplies a language-level sandbox; the subprocess provides crash isolation,
not an operating-system sandbox. Use additional OS isolation if the threat model
requires protection against interpreter vulnerabilities. Install a separate,
compatible Monty worker on the **server host**; it is not bundled with PSA:

```sh
cargo install monty-runtime --version 1.4.0 --locked
# Either put the installed `monty` executable on the server's PATH,
# or set its explicit path before starting the server:
export PSA_MONTY_WORKER="$HOME/.cargo/bin/monty"
cargo run -p psa_server -- start
```

PSA uses the external worker at `PSA_MONTY_WORKER` when set, otherwise it finds
`monty` on `PATH`. A bare worker name is searched on `PATH`; relative paths are
resolved against the server's working directory. Use exactly version 1.4.0 to match the server's Monty crates.
The worker is started on demand, so a missing or incompatible executable causes
CodeMode execution to fail rather than necessarily preventing server startup.

Add `"CodeMode"` to a bundle list alongside the bundles whose functions the
agent needs. For example, this optional server-default configuration enables
CodeMode and read-only file access:

```toml
[agent_defaults]
bundles = ["CodeMode", { FileTools = { read_only = true } }]

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
```

For an API configuration, the corresponding JSON field is:

```json
{"bundles": ["CodeMode", {"FileTools": {"read_only": true}}]}
```

Rust clients can use `BundleConfig::CodeMode` and
`BundleConfig::FileTools { read_only: true }`. Explicit lists replace inherited
defaults, so include any other desired bundles and hooks. CodeMode needs no
separate function configuration: participating installed bundles register their
functions automatically. `ToolDefinitions` publishes the available functions,
their calling conventions, and their generated argument schemas to the agent.
Tool-backed functions are async and are called with `await` and keyword arguments;
the built-in `PRIOR_RESULTS` function is synchronous and must not be awaited.
CodeMode calls are sequential barriers around other model-issued tool calls;
async functions within a script can still run in parallel with `asyncio.gather`.
For example:

```python
contents = await read(path="README.md")
print(contents)
```

CodeMode installs the read-only `PRIOR_RESULTS` function by default. Call
`PRIOR_RESULTS()`, `PRIOR_RESULTS(index=-1)`, or `PRIOR_RESULTS(-1)` **without
`await`** to get the exact text string of one previous committed `codemode`
invocation result, including errors and cancellations. Index `0` selects the
oldest result, `-1` the latest, and other negative indices count from the end.
Out-of-range indices raise `IndexError`; invalid arguments raise `TypeError`.
The current invocation, native results from other tools, and durable agent
completion `Result` events are excluded.

Results are loaded lazily from server trace history, including visible fork
ancestry. The archive is **not** copied into the Python checkpoint; only the small
function binding persists. Fetched strings that you retain in Python become
normal checkpoint data and can grow the checkpoint, so prefer printing or
processing inline instead of saving each result:

```python
print(PRIOR_RESULTS())       # Latest committed codemode result, no await.
print(PRIOR_RESULTS(0))      # Oldest visible codemode result.
print(len(PRIOR_RESULTS(-1)))  # Process inline without retaining the string.
```

This returns the stored tool-result text, with its original output bounds and
truncation intact; it cannot recover untruncated output beyond what was stored.
The read-only lookup itself has no external side effects.

Compatibility is limited to Monty's implemented Python features, not CPython's
complete language or APIs. The stdlib import allowlist, verified against Monty
1.4.0's `StandardLib::from_static` in `src/modules/mod.rs`, is:

- `asyncio`, `base64`, `binascii`, `bisect`, `collections`, `copy`, `csv`,
  `dataclasses`, `datetime`, `decimal`, `functools`, `hashlib`, `itertools`, `json`,
  `math`, `os`, `os.path`, `pathlib`, `posixpath` (an alias for `os.path`), `random`,
  `re`, `sys`, `time`, `tomllib`, `typing`, `unicodedata`.

Module attributes and object methods are incomplete: importing a module does not
promise its full CPython API. Imports of `os` or `pathlib` do **not** grant
filesystem access. There is no direct filesystem or network access; use installed
host functions for external operations. `globals()`, `dir()`, `vars()`, and
`__import__()` are unavailable. At REPL/module and function scope, `locals()` is a
snapshot, not a writable live namespace; inside `exec()`/`eval()` snippets it
instead exposes the snippet's namespace dictionary. `hasattr()` is not reliable
method/capability discovery and can raise `TypeError` for attributes that require
non-simple evaluation (such as calls or awaits).

Unknown function names raise `NameError` immediately, even without `await`.
Await tool-backed functions and all coroutines before finishing the script;
`asyncio.gather` supports independent parallel calls. Tool-backed functions return
text strings on success and raise exceptions on failure. `time.sleep()` waits
synchronously; `asyncio.sleep()` waits asynchronously and is cancellable. There is
no separate sleep duration cap; the script's wall timeout still applies. Prefer
`await asyncio.sleep(...)`.

Use `print()` for visible output; the final expression is not displayed. Printed
output is preserved even when execution fails, bounded to 12,000 characters;
when exceeded, its middle is replaced with a truncation marker. The total result,
including any exception, is bounded to 16,000 characters. Execution limits are:

- **60 seconds interpreter execution per feed**, excluding host waits.
- **660 seconds wall time per script once started**, including host waits but not
  time queued behind earlier calls.
- **64 MiB interpreter execution memory**, not a guaranteed persistent-state
  capacity or a whole-process memory limit.
- **16 MiB serialized checkpoint**. Serialization inflation varies with the
  stored values, and dumping can exhaust memory before reaching this size; a
  value fitting during execution is not guaranteed to checkpoint successfully.

Operations called through tool-backed functions retain their own restrictions
and can have external effects; the interpreter sandbox does not make those
effects safe or reversible.

Completed idle executions checkpoint REPL state in a server-private event attached
to the public tool result. Both are committed atomically for later calls, restart,
and forks at the relevant history boundary. Ordinary uncaught Python exceptions
fail the call but preserve earlier mutations when an idle checkpoint succeeds.
Interruptions, resource-limit failures, and failed checkpoint dumps discard only
the new state: the latest successful checkpoint is retained, not a full reset.
Restoration can still fail if that checkpoint is incompatible. Checkpoints retain
their interpreter and sleep policies: agents restored from older checkpoints may
still use the previous 10-second limits. Use `reset_codemode` or start a fresh
agent to adopt the current policy; forking an older checkpoint retains its older
policy. Do not depend on REPL state as the sole copy of important data. Forks do
not inherit live Bash process sessions, even if a saved variable refers to one;
external side effects are not rolled back.

To recover from bad state, a corrupt/incompatible checkpoint, or large retained
globals, call the separate native **`reset_codemode` tool with `{}` directly**,
not from Python. It is installed automatically by CodeMode and runs as a
sequential barrier. It must appear in the agent's tool snapshot: legacy agents
and their forks may need a fresh root agent because tool definitions are
snapshotted at agent creation, not automatically refreshed.
Reset appends a fresh, valid private `CodeModeSnapshot`,
clearing Python globals, imports, and functions; the next `codemode` call starts
fresh with the current policy (including the 60-second interpreter feed limit
and no separate sleep cap). The committed reset survives server restart.
Older checkpoints and tool results are preserved, and `PRIOR_RESULTS` history
remains available with the same visible-fork cutoffs. A historical fork before
the reset still sees its earlier checkpoint. If reset creation fails or is
cancelled, no new checkpoint is committed and the previous checkpoint remains.
Reset does not undo external filesystem, network, or process effects, and does
not stop live Bash sessions.

### Context compaction

Generated defaults install `Compaction {}` in `before_request`. It summarizes
older messages through hidden agents, then atomically restores system prompts,
tool definitions, a checkpoint, and the recent conversation. Configure
`reserve_tokens`, `keep_recent_tokens`, and optional `context_window` in the hook
payload. See [compaction](../../docs/COMPACTION.md) for behavior and limitations.

### Durable completion results

Runs can produce one durable `EventType::Result`: text through ordinary
`StopWithoutTools` or inactive Goal completion, or JSON through
`TypedResult { schema: ToolSchema, max_retries }`. These completion modes are
mutually incompatible, including inherited default hooks. Configuring none is
allowed but provides no automatic completion or Result. An interactive-only Goal
bundle falls back to `StopWithoutTools` on noninteractive agents.
Results are metadata excluded from model history.
Failure/cancellation does not synthesize results; accepted database saves cannot
be revoked and can complete after cancellation. Running goals continue even at
token limits. Non-running includes Blocked: a blocked goal stays retained/paused
while the current run settles Idle with a text Result.

TypedResult installs sequential `record_result` plus a require-result hook. Its
whole argument object must match the explicit object-root Draft 2020-12 schema
(local references only). Valid data atomically saves Result and Idle; invalid or
missing completion consumes correction retries (default three, then interrupt).
Clear an inherited `after_request` StopWithoutTools list when selecting the
bundle. See [durable result configuration and retrieval](../../docs/AGENT_RESULTS.md).

`GET /sessions/{sid}/agents/{aid}/results?limit=1` returns newest-first artifact
records over logical fork history. `EventsQuery.results_only` filters durable
Results before database pagination and during SSE durable delivery.

### Configurations, snapshots, and hook bindings

`POST /agent-configs` takes a JSON array of omission-aware `AgentConfigInput`,
upserting by `ConfigId { config_group, name }` after resolving server component
defaults. The client accepts either `AgentConfigInput` or normalized `AgentConfig`;
the latter sends every component list explicitly, including empty lists. Use
`AgentConfigInput::new(config_id, models)` to inherit defaults. `GET /agent-configs` lists
`AgentConfigStatus`; `POST /agent-configs/lookup` takes a `ConfigId` body and reads
its current configuration/status. `GET /agent-snapshots/{definition_id}` reads an
immutable `ResolvedAgentConfig` snapshot (JSON `null` when absent). Existing
agents and forks keep their pinned snapshots when current configurations change.
There is no compatibility API: `/agent-definitions` CRUD routes and client
methods are removed.

`POST /sessions/{session_id}/agents` takes `NewConfiguredAgent` with `config_id`
and a name, not a `definition_id`. Its optional `model: Option<ModelSelection>`
must exactly match a configured selector: `"Global"`, `"Parent"`, or a `Fixed`
object including both model and effort policy. Omitted/null selects the sole
choice, or the first choice when `default` is true. Registration/upsert normalizes
`default` to true for single-model configurations; multiple choices with
`default = false` require explicit selection. Model and effort inheritance resolve
at creation.

`POST /skills` uploads `NewSkill { config_id, tar_data }`, a complete skill tar
archive represented as JSON bytes. `GET /skills` and `GET /agent-configs` refresh
source metadata before listing; optional `?session_id=...` also refreshes that
session's selected project. Global files live in `<data_dir>/{agents,skills}`;
projects use the nearest probe-local `.agents/{agents,skills}`. Server-owned
skills are synchronized to managed remote-probe caches before agent creation.
For a probe advertising `host: true`, server-owned skills instead use their
original server paths without synchronization or cache writes. Probe-local skills
remain in place in either case. Skill records remain mutable even when agents
retain immutable definition snapshots.
For full types, enum encodings, TOML discovery, missing-component warnings,
selection rules and library storage configuration, see
[`../../docs/AGENT_DEF_SCHEMA.md`](../../docs/AGENT_DEF_SCHEMA.md).

Agent creation accepts `interactive`, defaulting to `false` when omitted from
JSON. It is creation-time metadata available to tools and hooks, not a runtime
behavior switch. Opt-in Goal and AskQuestion bundles consult it only when their
`interactive_only` construction flag is true; false enables them on any agent.
Agent responses expose it, and forks inherit the source flag.

Agent creation also accepts `hidden`, defaulting to `false` in JSON. It is client
visibility metadata only: the server stores and returns it without filtering
agents. Forks inherit the source flag.

`CreateSession.model_config: Option<ModelConfig>` is optional: omitted/null uses
server `session_defaults`, while an explicit configuration wins. Session responses
always contain the concrete persisted configuration, which `UpdateSession` can
replace explicitly. Reloading, updating, or forking never resets it to defaults.
Each input/normalized config supplies a nonempty `models` policy list (`Global`, `Parent`,
or `Fixed` with an effort policy), plus `description`, `selectable`, and inclusions.
Extra prompts are explicit `HookConfig::ExtraSystemPrompt { prompt }` payloads,
not a separate agent/config field. Resolution produces an internal executable
`AgentDefinition` with `model_config`, probe-local skill paths, and
`permitted_subagents` snapshot IDs; `ResolvedAgentConfig.references` separately
stores `SkillId` and subagent IDs. Skill paths are consumed only when `SkillsHook`
is explicitly configured. Metadata does not automatically select agents or inject
extra system prompts. The opt-in Subagents tool checks pinned permitted IDs when
creating children; trusted-client config-based creation remains unrestricted.
Unknown extra configuration/creation JSON fields are rejected.

Normalized configs and snapshots persist ordered `bundles`; omitted input
inherits server defaults, while explicit `[]` selects no bundles.
`BundleConfig::Noop` installs Noop. `BundleConfig::FileTools { read_only: false }`
installs Read, Edit, and Write; true installs only Read. FileTools requires a
payload, with `read_only` defaulting to false inside it; bare `"FileTools"` is
invalid. JSON uses `{"FileTools":{"read_only":true}}`, and TOML bundle entries
use `{ FileTools = { read_only = true } }`. `BundleConfig::Bash`
installs `Bash`, `InteractBash`, and `CloseBash`. `BundleConfig::CodeMode`
installs the `codemode` tool; see [CodeMode](#codemode) for its external worker
and optional bundle setup.
`{"Goal":{"interactive_only":true}}` installs `SetGoal`/`SetGoalStatus` plus stopping
policy; `{"AskQuestion":{"interactive_only":true}}` installs only `ask_question`,
with no hooks. These variants require payloads, not bare unit strings. The flag
defaults to true inside the payload: noninteractive agents get only
`StopWithoutTools` from Goal and nothing from AskQuestion. False fully enables
the bundle, including Goal state persistence and continuation on noninteractive
agents. Generated server defaults include Goal with `interactive_only = true`,
but not AskQuestion. TOML bundle entries use `{ Goal = { interactive_only = true } }`
or `{ AskQuestion = { interactive_only = true } }`. Low-level `query_client`
remains available to any agent independently of these installation flags.
`Subagents` installs child creation,
messaging, waiting, and listing backed by session-shared actor resources. These
tool groups require their bundle. See `../../docs/GOALS_AND_SUBAGENTS.md` for setup,
core ancestry, append-only goal semantics, and delivery/recovery limitations.
Bundles install in list order. Configured `agent_create` hooks run before
bundle creation hooks, so startup initializes context before bundle prompts.
Other stages run bundle hooks before explicit hooks. There is no standalone tool
configuration or agent/config/defaults `tools` field; every tool requires a bundle. Duplicate bundle IDs, tool IDs, or tool
names are rejected during component installation. The session manager retains
owners and instances by agent ID across worker retirement/failure. Forked agents
have independent resources; server restart does not restore runtime resources.
Shared session resources do not share tool instances or mutable hooks across
agents. Shutdown interrupts all workers, closes query/session coordination
services, then joins workers and cleans per-agent bundles in reverse order under
bounded timeouts. Probes stay
connected until cleanup finishes. See `../../docs/BASH_TOOLS.md` for process tools.
Omitting bundles during an omission-aware upsert uses the server default list;
explicit `[]` clears it. Existing executable snapshots are not changed.

Each normalized config uses `HookConfigs` with ordered `HookConfig` lists at `agent_create`,
`session_update`, `before_request`, and `after_request`. Omitted input stages
inherit independently from `[agent_defaults.hooks]`; explicit empty stages disable
those defaults. Without server defaults, all lists are empty. Selected bundles
can additionally supply their own hooks. Built-ins:

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
- `ExtraSystemPrompt { prompt }`: opt-in hook configuration that appends its
  explicit prompt as a separate `System` message (unless empty), including on
  forks. JSON uses `{"ExtraSystemPrompt":{"prompt":"Follow project conventions."}}`;
  no agent-level prompt field or empty default prompt hook is installed.
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
- `StopWithoutTools`: ordinary completion mode, normally bound to `after_request`;
  records a durable text Result and Idle for no-tool completion, including
  token-limited text. It is incompatible with Goal and TypedResult completion,
  including when inherited from defaults.
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
and return `ToolOutput { content, error }`. Neither trait requires `new`.
Construct stateless unit structs directly; stateful bundle components receive
explicit shared handles from their owner. Tools are constructed only by bundles;
`HookKind::from_config(&HookConfig)` and `Hooks::from_configs(&HookConfigs)`
dispatch construction from payload-ready configurations.
`BundleKind::from_config(&BundleConfig, Option<&SessionBundleHandle>)` returns
`Result<Self, InstallError>` and requires session resources for `Subagents`;
there is no unbound fallback. Runtime-owned resources are supplied by the runtime,
not serialized as configuration arguments. `ToolId` remains an identity enum;
there is no standalone tool configuration type.
The manager retains installed instances and bundle owners across worker retirement
and failure; resources are not globally shared or durably restored. Hook invocation uses
`&mut self`, allowing agent-runtime hook state across worker generations. The output flag is model-facing,
not an agent control signal. The worker persists each tool response separately. To interrupt,
a tool/hook explicitly saves an `Interrupted` status event. Both interfaces retain
cooperative cancellation and bounded cleanup. Session settings are invocation
snapshots; mailbox updates are applied after the invocation.

The worker resolves a fresh probe handle before each inference iteration and
interrupts without inference if the probe is missing. A mid-iteration disconnect
is reported through probe-operation errors; tools/hooks decide whether to change
status. Operations are never automatically retried.

`agents::sessions::Sessions` is the shared async facade for config batch upsert,
lookup/list and snapshot reads, skill upload/list, session create/query/update/fork,
configured agent create/query, request submission, steering, resume, interrupt,
and busy queries. `Runtime::handle` and `AgentContext::sessions()` expose it.
Registration uses `psa_api::AgentConfigInput`, normalized configs use
`psa_api::AgentConfig`, and executable snapshots use `psa_api::AgentDefinition`.
Session/agent operations
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
        model: Model::new(ProviderType::Openrouter, "example-model")?,
        effort: Effort::High,
    },
}).await?;
// Low-level cursor registration takes normalized configs, not server-default input.
let config: psa_api::AgentConfig = serde_json::from_value(serde_json::json!({
    "config_id": {"config_group": {"Custom": "examples"}, "name": "manual-replay"},
    "models": ["Global"]
}))?;
let config_id = config.config_id.clone();
cursor.upsert_agent_configs(vec![config]).await?;
let resolved = cursor.resolve_agent_config(
    &config_id, &std::collections::HashMap::new(), &std::collections::HashMap::new(),
).await?;
let agent = cursor.create_agent(session.id, NewAgent {
    subagent_parent_id: None,
    interactive: false,
    hidden: false,
    definition_id: resolved.definition.id,
    name: "Main".into(),
}).await?;
// Direct cursor operations do not execute lifecycle hooks; seed events explicitly.

cursor.add_events(agent.id, &[
    NewEvent::new(EventType::Model(ModelConfig {
        model: Model::new(ProviderType::Openrouter, "example-model")?,
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
// fork.agents is Vec<Agent>, including agents with empty streams.
// Each child agent.parent_id identifies its source agent.
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
outcome, usage, timing, and response metadata). Model configurations in `Model`
events and inference-result snapshots use `{ model: { provider, api_name }, effort }`;
there is no separate top-level provider. Persist the entire block atomically
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
sqlx migrate run --source runtime/psa_server/migrations
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

Generated `config.toml` contains root `host = true`. A probe reads this marker
from `config.toml` beside its selected `probe.toml`; sharing the server's config
directory and filesystem identifies the host probe. Missing markers default to
false, and existing server configs are not rewritten. The advertised `host` flag
is persisted and returned by probe discovery. At most one registered probe may
be host; the same UUID can reconnect, and advertising false relinquishes the role.
This is an authenticated configuration assertion, not automatic machine identity
verification. Do not copy `host = true` onto remote or isolated-filesystem probes.

See [the probe README](../psa_probe/README.md) for static builds, authentication,
probe discovery, and server-side remote operations. Sessions now require a
registered `probe_id`; forks inherit it. Workers resolve the session's probe
before inference and expose it to tools/hooks through `AgentContext::probe()`.
