This is a work in progress custom rust agent harness.

The focus is on centralized server, clients connect to it, and different probes can connect from different sandboxes or computers,
allowing managing agents across many different environments and seeing them in one central place.

Should also be easy to programatically interact with from a client library.

The codebase uses async code, not blocking sync.

# Crates

The following crates are present in the workspace. Runtime crates live under
`runtime/` (`psa_server`, `psa_agents`, `psa_components`, `psa_store`, `psa_config`, `psa_models`, `psa_codemode`, `psa_shared`, `psa_probe`), reusable libraries under `lib/`
(`psa`, `llm_api`, `actors`, `actors_macros`), and clients under `client/`:
- `client_shared`: UI-independent client projections, workspace state, and settings.
- `psa_gui`: shared Dioxus UI and a small platform-services interface; no native renderer.
- `psa_desktop`: desktop executable and settings persistence; enable `native` for the WebView renderer.
- `llm_api`: a generic wrapper over different llm api types, like OpenAI responses and completions, low level close to the api.
- `psa_server`: the HTTP/SSE API, probe WebSocket route, sandboxes, configuration discovery, and startup; hands the runtime its components.
- `psa_agents`: the agent runtime (supervisor, workers, agent loop, `AgentContext`) and the interfaces components implement; installs components only through a `ComponentFactory`.
- `psa_components`: the built-in bundles, tools, hooks, and codemode functions, and the `BuiltinComponents` factory.
- `psa_store`: SQLite persistence for the server (cursors, sessions, agents, events, configurations, migrations); independent of the agent runtime.
- `psa_config`: formats of data-directory files (layout, `auth.toml`, server `config.toml` schema and defaults, agent definition files); no runtime behaviour.
- `psa_models`: LLM provider clients from credentials (`ProviderRegistry`) and the persistent model catalog.
- `psa_codemode`: the Monty-based CodeMode engine and Python stub/result-class rendering; the server supplies checkpoints and host functions through `CodeModeHost`.
- `psa`: client library for the psa server. `psa::api` defines the wire types and routes clients and probes use, plus a thin HTTP/SSE `Client`; the crate root adds a higher-level layer (`Context`, session and agent handles, agent specs, event ranges) for programs driving agents.
- `psa_shared`: shared implementation utilities for the server and probe, including probe transport, local configuration locking, invocation `Cancellation`, and extraction of wire `Archive`s.
- `psa_probe`: standalone Linux probe providing remote file, directory, and process operations over an authenticated WebSocket.
- `actors`: provides async mailboxes and handles for actor-style message passing, with the `#[mailbox]` macro supplied by `actors_macros`.

# Tool authoring

Before adding or changing native agent tools, read
`.agent/skills/making-tools/SKILL.md`. Use typed arguments and the shared execution
macro and one concrete tool per file. Every tool must be installed by a Rust
bundle, even single-tool features; shared state and cleanup belong to that
bundle. There is no standalone tool configuration or explicit agent tool list.
Prefer shared middle truncation unless the
tool's specified behavior requires another policy. If existing capabilities are
insufficient, stop and propose the needed additions to the user
instead of expanding the harness to support the tool.

# Client library API stability

- The high-level `psa` API (everything at the crate root: `Context`, `SessionHandle`,
  `AgentHandle`, specs, event ranges, `Error`) is a stable surface for external
  programs. Do not change it unless the user asks. If a task seems to need a change
  there, stop and ask the user first, describing the change and why.
- `psa::api` (wire types, routes, raw `Client`) can change more freely, but keep it
  similar in shape: prefer additive changes over renames and restructuring.
- Before using or extending the high-level API, read
  `.agent/skills/psa-client/SKILL.md`, and keep that skill in sync with any
  approved change.

# Crate boundaries

`docs/CRATE_LAYOUT.md` defines what each crate owns, which way dependencies
point, and where new code belongs. Read it before adding modules, moving code,
or adding dependencies between workspace crates, and follow it.

- If a task seems to need a different boundary, stop and ask the user first.
  Describe the change and why. Examples of such changes are a new crate, a new
  dependency edge between workspace crates (for example `psa_probe` depending
  on server-side crates), or moving a responsibility to another crate.
- When the user approves a boundary change, update `docs/CRATE_LAYOUT.md` and
  the crate list above in the same change.

# Architecture

- Prefer actor-style designs where they make ownership, concurrency, and lifecycle management clearer, especially for long-lived tasks that own mutable state.
- Use the workspace's `actors` crate for mailbox-based communication between tasks. Keep actor state owned by its task and expose operations through messages and mailbox handles where appropriate.
- Keep simple operations as ordinary async functions when an actor would add unnecessary complexity.

# Coding Style

- Keep a clear main path through functions.
- Separate meaningful stages inside functions with blank lines. Keep closely related statements together.
- Extract a helper when its name explains a distinct operation, reduces difficult nesting, or removes meaningful duplication.
  - Avoid extracting helpers that require many arguments merely to pass local context around. Reconsider the extraction boundary; keep the operation inline when that is clearer. Don’t introduce a state struct solely to reduce argument count.
- Prefer early returns for invalid inputs and exceptional cases. Aim for no more than about three nested control-flow levels.
- Keep straightforward sequences, builders, dispatch matches, and linear tests together when splitting them would obscure their flow.
- A module with submodules lives in `module_name/mod.rs`, not `module_name.rs`
  next to a `module_name/` folder.
- Run `cargo fmt --all` and relevant tests after changes. Workspace builds require prepared static probe tools; see `runtime/psa_probe/README.md`.

## Unit test layout

Unit tests that live in their own file go in a tree under the crate's `src/tests/`
that mirrors the module path, not next to the code. Declare the test module in the
module it tests with a `#[path]` relative to that file:

```rust
// src/agents/supervisor.rs
#[cfg(test)]
#[path = "../tests/agents/supervisor/tests.rs"]
mod tests;
```

- The test module `name` declared in module `a::b` lives at `src/tests/a/b/name.rs`.
  For the crate root that is `src/tests/name.rs`. For a binary root such as
  `main.rs`, the tree starts at `src/tests/main/`.
- A test module with file submodules uses `src/tests/a/b/name/mod.rs`, with its
  submodules next to it in that folder.
- Name test modules `tests` or `<topic>_tests`. Test-only helper modules
  (`test_support`, `testing`) follow the same layout.
- Tests that only need the public API go in the crate's top-level `tests/`
  folder as integration tests.

## Naming

All names should follow appropriate rust styling (snake_case vs UPPER_SNAKE_CASE vs UpperCamelCase for different types of names)

- Functions or Methods
  - Operations which are generally understood to have a certain name (such as common operations on data structures, files, etc.) can be named according to common conventions rather than following these exact rules.
  - Keep familiar Rust and lifecycle names such as `new`, `as_str`, `into_output`, `close`, `flush`, and `next_event`. Simple error factories and test fixtures may use concise noun-style names. Use consistent terminology for equivalent operations across modules.
  - Name functions for what they do, distinguishing access, validation, construction, and mutation. Prefer a specific operation and object over vague names such as `process` or bare nouns for actions. Let the receiver/module supply context when repeating it would add noise.
  - In general a prefix describing an action the function does is a good idea. The prefixes below are good recommendations, not hard rules. Choose better prefixes when applicable:
    - Use noun-style names for accessors, with `_mut` for mutable access. Accessors should not create missing state; use `ensure_*` when initialization may occur.
    - Use `is_*`, `has_*`, `can_*`, or `matches_*` for side-effect-free boolean queries.
    - Use `validate_*` to check input validity and `require_*` to check a specific precondition. These return errors rather than repairing state. Reserve `ensure_*` for establishing a condition, such as creating a missing item.
    - Use `parse_*` to interpret external data into values without changing application state, `serialize_*`/`encode_*` to produce wire representations, and `build_*` to assemble composite values. Keep conventional codec names such as `decode` where appropriate.
    - Make mutation semantics explicit: `append_*` adds after existing data, `merge_*` combines partial/keyed information, `apply_*` incorporates an incoming event/delta/snapshot, and `update_*` refreshes a named destination. Don't call an operation `append` or `replace` if it also performs materially different work.
    - Use `handle_*` for event orchestration when no more specific operation describes it. Don't add generic action prefixes mechanically.
