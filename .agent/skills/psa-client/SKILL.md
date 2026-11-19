---
name: psa-client
description: Drive PSA agents from a Rust program with the high-level psa client API (Context, SessionHandle, AgentHandle, AgentSpec/AgentLayout, Skill, EventRange). Use when writing programs, tests, or tools that create sessions and agents, send messages, wait for runs, read results, or query events.
---

# PSA high-level client API

The `psa` crate (`lib/psa`) has two layers:

- `psa::api`: wire types, route templates, and a thin HTTP/SSE `api::Client`
  with one method per route. Use it only for things the high-level layer lacks
  (`context.client()` returns it).
- The crate root: a higher-level layer that hides config IDs, probe IDs, paging,
  SSE reconnection, and run tracking. Prefer it for programs that drive agents.

The high-level API is a stable surface. Do not change it unless the user asks;
if a task seems to need a change, stop and ask the user first.

Sources: `lib/psa/src/{context,probe,session,agent,spec,skill,events,error}.rs`. An
end-to-end example is `lib/psa/examples/run_agent.rs`; server-backed tests are
in `runtime/psa_server/tests/event_loop/library.rs`.

## Quick start

```rust
use psa::{AgentLayout, AgentSpec, Context, SessionSpec};

let context = Context::connect("http://localhost:8080", client_key, "my-app")?
    .working_directory("/work/my-app") // default cwd for new sessions
    .build()
    .await?;
let session = context.ensure_session(SessionSpec::new("triage")).await?;
let agent = session
    .ensure_agent(AgentSpec::new("triager", AgentLayout::new("triager")))
    .await?;
let summary: String = agent.run_as("Summarize the open issues.").await?;
```

All operations are async and return `psa::Result<T>` (`psa::Error`).

## Context

`Context::connect(url, key, project)?` or `Context::builder(api_client, project)`
returns a `ContextBuilder`. The project is required: it is the project of every
session the context creates and lists, and `ConfigGroup::Custom(<project>)` is
the group agent layouts are registered in.

| Builder method | Meaning | Default |
|---|---|---|
| `.probe(ProbeSelector)` | Restrict to this probe and the sandboxes it manages; also the default probe for sessions and the manager of new sandboxes | no restriction; host probe is the default |
| `.working_directory(path)` | Default session cwd | the session probe's home directory |

`ProbeSelector`: `Host`, `Id(ProbeId)`, `Label(..)`, `Hostname(..)`,
`Sandbox(name)`. Exactly one usable probe must match, otherwise
`ProbeNotFound` / `AmbiguousProbe`. `.build().await` validates the project and
resolves the restriction. `Context` is cheap to clone.

Sessions: `sessions()` (all in the project on usable probes, including forks,
newest first), `find_session(name)`, `session(id)`, `create_session(spec)`,
`ensure_session(spec)` (find by exact name or create; an existing session's
probe is not touched, call `ensure_probe_up`).

Configs: `register_layout(&AgentLayout)` and `register_skill(&Skill)` register
into the context's group and return the `ConfigId`; see Agent specs.

Probes: `probes()` (usable probes and sandboxes, without deleted sandboxes),
`find_probe(selector)`, `probe(id)` (`ProbeNotAllowed` outside the restriction),
`default_probe()`, `create_sandbox(&CreateSandbox)` (on the default probe; does
not wait for it to connect). Accessors: `client()`, `project()`,
`config_group()`, `probe_restriction()`, `working_directory()`.

`SessionSpec::new(name)` with `.probe(selector)`, `.new_sandbox(CreateSandbox)`,
`.working_directory(..)`, `.model(ModelConfig)`, `.decision_model(Model)`. The
probe (`SessionProbe::Default` / `Select` / `NewSandbox`) is brought up before
the session is created.

## Probes

`ProbeHandle` holds the probe and sandbox records it was loaded with: `id()`,
`info()` (absent for a sandbox whose probe never connected), `sandbox()`,
`is_sandbox()`, `is_live()`, `reload()`.

- `ensure_up()`: a live probe is fine; a stopped sandbox is started and waited
  for (up to two minutes, `ProbeStartTimeout`); a running sandbox is waited for;
  a missing, deleted or unknown sandbox is `SandboxUnavailable`; an offline plain
  probe is `ProbeOffline`.
- `stop()`: stop the sandbox container; does nothing for a plain probe.

## Sessions

`SessionHandle` holds the session snapshot it was loaded with (`id()`, `name()`,
`info()`; `reload()` refreshes it).

- `agents()`, `agent(name)` (top-level agent with that exact name).
- `create_agent(AgentSpec)`, `ensure_agent(AgentSpec)` (find by name or create;
  an existing agent keeps its definition even if the layout changed).
- `rename(name)`, `update(&UpdateSession)`, `fork(ForkPoint)`.
- `probe()`, `ensure_probe_up()` (start the session's sandbox if needed),
  `stop_probe()` (stop its sandbox; no-op on a plain probe).
- `refresh_configs()`: rescan global configs and this session's `.agents` dir.
- `wait_idle()`: wait until the session has no active work.
- `events`, `event_pages`, `stream`: see Event ranges (session positions).

## Agent specs

`AgentSpec::new(agent_name, config)` with `.model(ModelSelection)`,
`.interactive()`, `.hidden()`. `config` is an `AgentConfigRef`:

- A name (`"reviewer"`, `AgentConfigRef::Named`): an existing config, looked up
  in the context's group first, then `Global`. Unknown names fail with
  `Error::UnknownConfig`.
- An `AgentLayout` (`AgentConfigRef::Layout`): registered with
  `Context::register_layout` right before the agent is created, replacing a
  config of the same name.

`AgentLayout::new(config_name)` defaults to following the session model
(`ModelSelection::Global`), the server's default bundles, and no
subagents or skills. Builders: `.description(..)`, `.models(vec)` (first is the
default), `.bundles(vec)` (exact list, replaces defaults; bundles install all tools and
hooks, so include `BundleConfig::Startup` and a completion bundle such as
`StopWithoutTools` when replacing them),
`.subagents(Inclusions)`, `.skills(Inclusions)`.

`Inclusions<R>` (`R` is `AgentConfigRef` for subagents, `SkillRef` for
skills): `Inclusions::default()` with `.include_global()` (every item of
`Global`), `.include_group()` (every item of the context's own group), and
`.include(item)`: a name (resolved own group first, then Global; unknown names
fail with `UnknownConfig` / `UnknownSkill`), or an inline `AgentLayout` /
`Skill`.

`register_layout` walks the layout tree: every inline layout and skill,
however nested, is registered in the context's group, and named inclusions
resolve against them too (so a layout can include itself by name). An inline
name defined twice must be equal, else `ConflictingConfig` /
`ConflictingSkill`; other name clashes are the caller's problem. Names are
resolved before anything is uploaded; then skills are uploaded and all layouts
are upserted in one atomic batch.

`Skill::from_path(dir).await` packages a directory holding `SKILL.md` into a
reproducible, zstd-compressed tar archive (up to 256 MiB uncompressed), following symlinks (cycles and special files fail
with `InvalidSkill`). Its name is the `SKILL.md` metadata name, else the
directory name. Accessors: `name()`, `description()`, `archive()`,
`archive_sha256()` (of the uncompressed tar). A skill whose archive hash matches the registered one is not
uploaded again. `SkillRef` is `Named(String)` or `Skill(Skill)`; `&str`,
`String`, and `Skill` convert into it.

## Agents

`AgentHandle` holds the agent snapshot it was loaded with (`id()`,
`session_id()`, `name()`, `state()`, `usage()`, `info()`; `reload()`).

Messages take `impl Into<Content>`: `&str`, `String`, or `Vec<ContentBlock>`.

| Method | Effect |
|---|---|
| `send(content)` | Queue after any current run; starts a run |
| `steer(content)` | Deliver at the next boundary, even mid-run |
| `resume(content)` | Resume an interrupted agent |
| `send_with(delivery, content)` | Explicit `MessageDelivery` |
| `interrupt()` | Interrupt the current run |
| `command(AgentCommand)` / `command_with(delivery, ..)` | Run a command and wait for its `CommandResult` |
| `run(content)` / `run_with(delivery, content)` | Send and wait for the run to end; returns `RunOutcome { state, result }` |
| `run_as::<T>(content)` | `run`, then decode the run's result into `T` (`NoResult` if none) |
| `results(limit)`, `last_result_as::<T>()` | Durable results, newest first |
| `pending_queries()`, `respond(query_id, &response)` | Answer client queries (e.g. ask-question) |
| `model_usage()`, `context_state()` | Per-model usage; current model and context usage |

`run` opens the agent's stream before sending, then waits for a committed
`StatusChange` to `Running` followed by one to `Idle`/`Interrupted`; runs that
end before its own starts are ignored. A failed request returns
`Error::RequestFailed`. `RunOutcome::result` is the newest result recorded after
the message was sent. A queued message to an interrupted agent is rejected by
the server; use `resume` / `run_with(MessageDelivery::Resume, ..)`.

## Event ranges

`EventRange { start: EventStart, end: EventEnd, results_only, include_pending }`.
Cursors are agent event IDs on agents and session positions on sessions.

| `EventStart` | Starts at |
|---|---|
| `Beginning` | first event |
| `CurrentContext` | start of the agent's current model context (agents only) |
| `ContextOf(n)` | start of the context containing event `n` (agents only) |
| `After(n)` / `At(n)` | just after / at cursor `n` |
| `Head` | only events committed from now on |
| `Last(n)` | newest `n` events (newest `n` results with `results_only`) up to the end |

| `EventEnd` | Ends at |
|---|---|
| `Head` | the end when the range is read |
| `Through(n)` | cursor `n`, inclusive |
| `Follow` | never; keeps following (stream only) |

Constructors and builders: `EventRange::all()`, `::current_context()`,
`::last(n)`, `::follow()`, `.starting(start)`, `.ending(end)`, `.following()`,
`.results_only()`, `.with_pending()`.

Reading (on both `AgentHandle` and `SessionHandle`):

- `events(range) -> Vec<EventEnvelope>`: all committed events of a finite range.
- `event_pages(range) -> EventPages`: the same, fetched lazily page by page.
- `stream(range) -> EventFeed`: a `Stream` of `EventUpdate`s, connected before
  it returns. It replays the range, sends `Live { pending, queries }` (replace
  all provisional state and outstanding queries with these), then live
  `Committed`, `Pending`, `PendingRemoved`, `QueryRequested`, `QueryRemoved`,
  `Agent`, `Session`, and `RequestFailed` updates. It reconnects by itself after
  a server resync or when a following stream ends; a transport error ends it.

`events`/`event_pages` with `EventEnd::Follow`, or context bounds on a session,
fail with `Error::InvalidRange`. Provisional events only appear in streams.

## Errors

`psa::Error`: `Api(ClientError)` (transport/HTTP), `InvalidProject`,
`ProbeNotFound`, `AmbiguousProbe`, `ProbeNotAllowed`, `ProbeOffline`,
`SandboxUnavailable`, `ProbeStartTimeout`, `MissingWorkingDirectory`, `UnknownConfig`,
`UnknownSkill`, `ConflictingConfig`, `ConflictingSkill`, `InvalidSkill`, `InvalidRange`, `RequestFailed`, `StreamEnded`, `NoResult`,
`InvalidResult` (result payload did not decode into `T`).
