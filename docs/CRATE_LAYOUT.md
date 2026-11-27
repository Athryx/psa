# Crate layout

This document says which crate owns what, which way dependencies point, and
where new code belongs. `AGENTS.md` requires agents to follow it. Changing a
boundary described here (a new crate, a new dependency edge between workspace
crates, or moving a responsibility to another crate) needs the user's approval
first.

## Layers

Dependencies point downward only. A crate may depend on crates in lower
layers, and only on the listed same-layer crates.

```text
1. binaries      psa_server, psa_probe, psa_desktop, psa_cli, trace_cli
2. components    psa_components
3. runtime       psa_agents
4. services      psa_models, psa_codemode                 psa_gui
5. foundations   psa_store, psa_config                    client_shared
6. utilities     psa_shared
7. libraries     psa, llm_api, actors, actors_macros, agent_trace
```

`psa_probe` is a binary, but `psa_server` also embeds it as a library to run
its local probe. Clients (right-hand column) depend only on `psa`,
`llm_api`, and `psa_config` from the lower layers. They use `psa_config` only
for the data-directory layout and file formats of a server or probe on the
same machine, for example to find the local control socket. A client
connected to a remote server uses only `psa::api`.

Current edges between runtime crates:

| Crate | Depends on (workspace crates) |
| --- | --- |
| `psa_shared` | `psa`, `actors` |
| `psa_config` | `psa`, `llm_api`, `psa_shared` |
| `psa_store` | `psa`, `llm_api` |
| `psa_models` | `psa`, `llm_api`, `actors`, `psa_shared`, `psa_config`, `psa_store` |
| `psa_codemode` | `psa_shared`, `psa_store` (snapshot size limit only) |
| `psa_probe` | `psa`, `actors`, `psa_shared`, `psa_config` |
| `psa_agents` | `psa`, `llm_api`, `actors`, `psa_shared`, `psa_config`, `psa_store`, `psa_models`, `psa_codemode` |
| `psa_components` | `psa_agents` and everything it depends on, plus `agent_trace` |
| `psa_server` | all of the above |

Client edges: `client_shared` and `psa_desktop` depend on `psa_config`
(besides `psa` and `llm_api`), and `psa_cli` depends on `client_shared`,
`psa_config`, `psa`, and `llm_api`. No client depends on any other runtime
crate.

These rules matter most:

- `psa_probe` runs in remote environments and sandboxes, and it is also built
  as a static binary. It must never depend on `psa_server`, `psa_agents`,
  `psa_components`, `psa_store`, `psa_models`, or `psa_codemode`. Its tests
  may use them as dev-dependencies.
- `psa_agents` never depends on `psa_components`, and never names a concrete
  component. It installs components only through the `ComponentFactory` the
  server hands it (see "Component seam" below).
- Crates below `psa_server` must not depend on it. When one needs something
  from the server, define a small trait or data type in the lower crate and
  implement it in the server. `CodeModeHost` in `psa_codemode` is an example.

## Ownership

| Crate | Owns | Does not own |
| --- | --- | --- |
| `llm_api` | Provider wire formats and inference requests, close to each API | PSA concepts, credentials storage |
| `psa` | The stable high-level client API at the crate root; `psa::api` wire types, routes, and raw HTTP/SSE client shared by server, probe, and clients | Server behaviour |
| `actors` | Mailboxes, handles, `#[mailbox]` | Domain logic |
| `psa_shared` | Small utilities used by both server and probe: probe transport, config file locking, invocation `Cancellation`, reading and extracting wire `Archive`s | Anything with a single user (put it in that crate instead) |
| `psa_config` | Formats of files in the data directory: layout and file names, `auth.toml`, server `config.toml` schema and defaults, agent definition files (`agents/*.toml`) | CLI parsing and startup resolution, discovery of config files on probes, validation that needs the runtime (bundle compatibility) |
| `psa_store` | SQLite persistence: migrations, cursors, records for sessions, agents, events, configurations, models; offline SQLx metadata | Validation of runtime rules, agent behaviour |
| `psa_models` | `ProviderRegistry` (provider clients from credentials, ChatGPT device sign-in and token refresh) and the model catalog (sync, refresh, pins) | The `auth.toml` format (uses `psa_config::auth`) |
| `psa_codemode` | The Monty CodeMode engine, Python stubs, result-class preludes, the `CodeModeHost` seam | Which host functions exist and what they do (server) |
| `psa_probe` | The standalone probe: remote file, directory, process, and sandbox operations; `probe.toml` | Server-side probe bookkeeping |
| `psa_agents` | The agent runtime: supervisor, per-agent workers and the agent loop, `AgentContext`, `RuntimeHandle`, projection and publication, client queries, the live probe connection registry; the component interfaces (`Tool`, `Hook`, `CommandHook`, `Bundle`, `CodeModeFunction`, `Installer`, `InstalledComponents`), the `ComponentFactory` and `ConfigDiscovery` seams, shared harness prompts; test fixtures behind the `testing` feature | Concrete components, HTTP, probe transport, configuration discovery |
| `psa_components` | Built-in bundles, tools, hooks, and codemode functions; session-shared resources (subagent coordinator, decision models); bundle validation; trace export; the `BuiltinComponents` factory | When and how components run, persistence ownership |
| `psa_server` | HTTP/SSE API, the probe WebSocket route, sandboxes, config discovery (`ConfigRegistry`, which implements `ConfigDiscovery`), CLI and startup | The agent runtime, concrete components, persistence details, file formats, provider plumbing, the CodeMode engine |
| `client_shared`, `psa_gui`, `psa_desktop` | Client projections, UI, desktop shell; in `client_shared`, the slash command registry and completion menus for every front end, the root agent choice before a new session's first message (`root_draft`), local server control (`LocalServerManager`), and executable lookup (`executables`) | Anything the server or probe needs |
| `psa_cli` | The `psa` command: the ratatui terminal client (`tui`), and subcommands that launch and manage local server, desktop, and probe processes | Server, probe, or GUI behaviour (it runs their executables); client projections shared with the GUI (those live in `client_shared`) |
| `agent_trace`, `trace_cli` | Harness-neutral trace format and tools | PSA runtime types |

## Where new code goes

Ask these questions in order:

1. **Is it a wire type or route that clients or probes see?** Put it in
   `psa::api`. Changes to the high-level `psa` crate root need the user's
   approval (see `AGENTS.md`).
2. **Is it the format of a file a user edits or that processes share?** Put it
   in `psa_config`. This covers the schema, defaults, and read/write helpers.
   The code that decides when to read the file stays with its caller.
3. **Is it SQL or a stored record?** Put it in `psa_store`. If the record is
   checked by a runtime rule, the caller does the check before writing, as
   the server does with `validate_completion_config`.
4. **Is it about LLM providers, credentials in memory, or model metadata?**
   Put it in `psa_models`. Raw provider protocols go in `llm_api`.
5. **Is it the CodeMode interpreter or Python-facing rendering?** Put it in
   `psa_codemode`, and reach server state through `CodeModeHost`.
6. **Does it run on the probe side?** Put it in `psa_probe`. If the server
   and probe share it and it is small, put it in `psa_shared`.
7. **Is it a concrete tool, hook, bundle, or codemode function?** Put it in
   `psa_components`, following `.agent/skills/making-tools/SKILL.md`.
8. **Is it agent runtime behaviour**, such as scheduling, the worker loop, an
   `AgentContext` capability, or a component interface? Put it in
   `psa_agents`. Changes to what components can do are proposals, as the
   making-tools skill describes.
9. **Otherwise** it is HTTP, transport, or startup behaviour, and it belongs in
   `psa_server`.

Within a crate, keep one concern per module and keep the module tree
matching the ownership above, for example `{bundles,tools,hooks}` in
`psa_components`.

## Component seam

The runtime stores installed components as trait objects (`Box<dyn DynTool>`,
`Box<dyn DynHook>`, `Box<dyn DynBundle>`, `Box<dyn CodeModeFunction>`).
Component authors implement the async traits (`Tool`, `Hook`, `CommandHook`,
`Bundle`); blanket impls provide the object-safe forms. There are no enums of
components in either crate.

`ComponentFactory` (in `psa_agents::factory`) is the only way the runtime
obtains components. It validates bundle selections, decides which commands a
definition supports, starts session-shared components, and installs an
agent's bundles. `psa_components::bundles::BuiltinComponents` implements it;
`psa_agents::bundles::testing::TestComponents` is a fake for runtime tests.

`ConfigDiscovery` (in `psa_agents::config_discovery`) is how the runtime reads
`.agents` directories and skill sources; the server's `ConfigRegistry`
implements it.

Tests of the runtime use `psa_agents` fakes. Tests of concrete components live
in `psa_components` and use the `psa_agents` `testing` feature's fixtures.
Tests needing the HTTP API or the probe WebSocket route live in `psa_server`.
