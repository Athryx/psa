# psa_config

Formats of the files a user edits or that PSA processes share in the data
directory. The crate holds serde schemas, defaults, and careful read/write
helpers. It does not hold runtime behaviour.

| Module | File | Used by |
| --- | --- | --- |
| `paths` | data-directory layout and file names, including the server's lock, control-socket, and log paths | server, probe, local clients |
| `auth` | `auth.toml`: `[psa]` probe/client keys and provider credential tables | server, probe, `psa_models` |
| `server` | `config.toml`: server schema, generated default config and agents, `AgentDefaults` | server, probe (`host` marker only) |
| `agent_definition` | `agents/*.toml` agent definitions and their inclusion rules | server config discovery |

`docs/AGENT_DEF_SCHEMA.md` documents the agent definition format for users.

## Boundaries

- Allowed dependencies: `psa::api` and `llm_api` types, serde/toml, and
  `psa_shared` for the shared file lock. The crate must not depend on the
  database, probes, providers, or the agent runtime.
- Command-line parsing and startup resolution, such as combining `--data-dir`
  and `--listen` with a file, stay in each binary.
- Checks that need runtime knowledge stay with their owner. For example,
  bundle compatibility (`validate_completion_config`) is checked by the
  server after parsing.
- `auth.toml` errors never keep TOML diagnostics, because their source
  excerpts can contain credentials.
