This is a work in progress custom rust agent harness.

The focus is on centralized server, clients connect to it, and different probes can connect from different sandboxes or computers,
allowing managing agents across many different environments and seeing them in one central place.

Should also be easy to programatically interact with from a client library.

The codebase uses async code, not blocking sync.

# Crates

The following crates are present in the workspace:
- `llm_api`: a generic wrapper over different llm api types, like OpenAI responses and completions, low level close to the api.
- `psa_server`: main server managing sessions and calling into `llm_api` to make inference requests.
- `psa_api`: defines common types and endpoints clients and probes use to communicate to psa server, and provides a thin client library for making the requests.

# Coding Style

- Keep a clear main path through functions.
- Separate meaningful stages inside functions with blank lines. Keep closely related statements together.
- Extract a helper when its name explains a distinct operation, reduces difficult nesting, or removes meaningful duplication.
  - Avoid extracting helpers that require many arguments merely to pass local context around. Reconsider the extraction boundary; keep the operation inline when that is clearer. Don’t introduce a state struct solely to reduce argument count.
- Prefer early returns for invalid inputs and exceptional cases. Aim for no more than about three nested control-flow levels.
- Keep straightforward sequences, builders, dispatch matches, and linear tests together when splitting them would obscure their flow.
- Run `cargo fmt --all` and relevant tests after changes.

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
