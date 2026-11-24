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
1. binaries      psa_server, psa_probe, psa_desktop, trace_cli
2. services      psa_models, psa_codemode                 psa_gui
3. foundations   psa_store, psa_config                    client_shared
4. utilities     psa_shared
5. libraries     psa, llm_api, actors, actors_macros, agent_trace
```

`psa_probe` is a binary, but `psa_server` also embeds it as a library to run
its local probe. Clients (right-hand column) depend only on `psa` and
`llm_api` from the lower layers.

Current edges between runtime crates:

| Crate | Depends on (workspace crates) |
| --- | --- |
| `psa_shared` | `psa`, `actors` |
| `psa_config` | `psa`, `llm_api`, `psa_shared` |
| `psa_store` | `psa`, `llm_api` |
| `psa_models` | `psa`, `llm_api`, `actors`, `psa_shared`, `psa_config`, `psa_store` |
| `psa_codemode` | `psa_shared`, `psa_store` (snapshot size limit only) |
| `psa_probe` | `psa`, `actors`, `psa_shared`, `psa_config` |
| `psa_server` | all of the above, plus `agent_trace` |

Two rules matter most:

- `psa_probe` runs in remote environments and sandboxes, and it is also built
  as a static binary. It must never depend on `psa_server`, `psa_store`,
  `psa_models`, or `psa_codemode`. Its tests may use them as dev-dependencies.
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
| `psa_models` | `ProviderRegistry` (provider clients from credentials) and the model catalog (sync, refresh, pins) | The `auth.toml` format (uses `psa_config::auth`) |
| `psa_codemode` | The Monty CodeMode engine, Python stubs, result-class preludes, the `CodeModeHost` seam | Which host functions exist and what they do (server) |
| `psa_probe` | The standalone probe: remote file, directory, process, and sandbox operations; `probe.toml` | Server-side probe bookkeeping |
| `psa_server` | HTTP/SSE API, agent runtime and supervisor, agent manager, bundles/tools/hooks, probe registry, config discovery (`ConfigRegistry`), CLI and startup | Persistence details, file formats, provider plumbing, the CodeMode engine |
| `client_shared`, `psa_gui`, `psa_desktop` | Client projections, UI, desktop shell | Anything the server or probe needs |
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
7. **Otherwise** it is agent runtime, tool, hook, bundle, or HTTP behaviour,
   and it belongs in `psa_server`. Native tools follow
   `.agent/skills/making-tools/SKILL.md`.

Within a crate, keep one concern per module and keep the module tree
matching the ownership above, for example `components/{bundles,tools,hooks}`
in the server.

## Not split yet

Agent core and concrete components (tools, hooks, bundles) are still both in
`psa_server`, because components are dispatched through the `ToolKind` and
`HookKind` enums. Splitting them would need dynamic dispatch or a registry,
and it needs the user's approval before work starts.
