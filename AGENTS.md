This is a work in progress custom rust agent harness.

The focus is on centralized server, clients connect to it, and different probes can connect from different sandboxes or computers,
allowing managing agents across many different environments and seeing them in one central place.

Should also be easy to programatically interact with from a client library.

# Coding Style

- Keep a clear main path through functions.
- Separate meaningful stages inside functions with blank lines. Keep closely related statements together.
- Extract a helper when its name explains a distinct operation, reduces difficult nesting, or removes meaningful duplication.
  - Avoid extracting helpers that require many arguments merely to pass local context around. Reconsider the extraction boundary; keep the operation inline when that is clearer. Don’t introduce a state struct solely to reduce argument count.
- Prefer early returns for invalid inputs and exceptional cases. Aim for no more than about three nested control-flow levels.
- Keep straightforward sequences, builders, dispatch matches, and linear tests together when splitting them would obscure their flow.
- Run `cargo fmt --all` and relevant tests after changes.
