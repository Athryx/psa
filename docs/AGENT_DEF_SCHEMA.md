# Agent configuration and definition snapshots

## Implemented API and identity

The configuration API is implemented in `psa_api/src/definitions.rs`, with HTTP
routes in `psa_server/src/http.rs` and a thin `psa_api::Client`. Discovery is in
`psa_server/src/config_registry`; registration and immutable graph resolution are
in `psa_server/src/db/configurations.rs`.

- External identity is `ConfigId { config_group, name }`, separately namespaced
  for agents and skills. Upsert replaces the current configuration for that ID.
- `AgentConfig` is unresolved, mutable source configuration. Agent creation pins
  an immutable executable `AgentDefinition`, identified by `AgentDefinitionId`.
  Existing agents retain their snapshots after configuration changes.
- `SkillId` is a stable mutable record ID, not a content revision. Snapshots can
  retain a skill ID while its metadata or files change. Immutable skill records
  and historical content are not promised.
- File discovery and API registration converge on shared validation, batch
  registration, inclusion selection, and snapshot resolution. File reading and
  server-to-probe materialization are adapters, not separate definition systems.

### HTTP routes and client methods

| Route | Input | Response | `Client` method |
| --- | --- | --- | --- |
| `POST /agent-configs` | JSON array of `AgentConfig` | `Vec<AgentConfigStatus>` | `upsert_agent_configs(&[AgentConfig])` |
| `GET /agent-configs?session_id=...` | Optional session query | `Vec<AgentConfigStatus>` | `list_agent_configs(Option<SessionId>)` |
| `POST /agent-configs/lookup` | `ConfigId` JSON body | `AgentConfigStatus` | `get_agent_config(&ConfigId)` |
| `GET /agent-snapshots/{definition_id}` | Internal snapshot ID | `Option<ResolvedAgentConfig>` | `get_resolved_agent_config(AgentDefinitionId)` |
| `POST /skills` | `NewSkill` JSON body | `Skill` (201) | `upload_skill(&NewSkill)` |
| `GET /skills?session_id=...` | Optional session query | `Vec<Skill>` | `list_skills(Option<SessionId>)` |
| `POST /sessions/{session_id}/agents` | `NewConfiguredAgent` | `Agent` (201) | `create_agent(SessionId, &NewConfiguredAgent)` |

Lookup by compound config ID uses a body rather than path encoding. An unknown
snapshot returns JSON `null`; a missing current config produces a not-found
error. Lists refresh source metadata first; `session_id` selects the project to
refresh, not a filter that hides other registered groups. Snapshot reads and
current-config lookup do not perform discovery.

There is **no backwards compatibility**: `/agent-definitions` CRUD routes,
constants, and client methods are removed. The old `CreateAgent` body is not
accepted by the agent-creation HTTP route. Internal pinned-definition creation
remains available to the subagent runtime, not as an alternate HTTP API.

## Current configuration and skill types

The following field summaries use the actual Rust API names:

```rust
struct ConfigId { config_group: ConfigGroup, name: String }
enum ConfigGroup {
    Custom(String),
    Global,
    Project { path: String, probe_id: ProbeId },
}
struct IncludedItems { groups: Vec<ConfigGroup>, items: Vec<ConfigId> }

struct AgentConfig {
    config_id: ConfigId,
    description: String,
    selectable: bool,
    bundles: Vec<BundleId>,
    tools: Vec<ToolId>,
    hooks: HookIds,
    extra_system_prompt: String,
    models: Vec<ModelSelection>,
    default: bool,
    subagents: IncludedItems,
    skills: IncludedItems,
}
enum ModelSelection {
    Fixed { model: String, effort: EffortSelection },
    Global,
    Parent,
}
enum EffortSelection { Fixed(Effort), Parent, Global }

struct Skill {
    id: SkillId,
    config_id: ConfigId,
    name: String,
    description: String,
    skill_location: SkillLocation,
    disable_model_invocation: bool,
}
enum SkillLocation {
    Global(PathBuf),
    Probe { id: ProbeId, path: PathBuf },
}
struct NewSkill { config_id: ConfigId, tar_data: Vec<u8> }
```

`SkillLocation::Global` means server-owned storage, independent of the skill's
`ConfigGroup`. `SkillLocation::Probe` means probe-owned files read in place;
they are never copied to another probe. Cross-probe use fails explicitly.
`SkillRegistration` is the trusted library/discovery registration form with
parsed metadata and location; HTTP upload callers cannot assert server paths.

Uploads contain an uncompressed tar of the complete skill directory, including
supporting files, with `SKILL.md` at its root. `tar_data` serializes as a JSON
array of bytes, not base64 or multipart data. Archives are limited to 64 MiB
and 10,000 entries; the skills route allows the larger JSON byte-array envelope.
Links, special files, escaping and
duplicate archive paths are rejected. Metadata uses the same parser as discovery;
its name must match `config_id.name`. `disable-model-invocation` is preserved.

### Batch JSON example

`POST /agent-configs` takes an array, even for one config:

```json
[
  {
    "config_id": {"config_group": {"Custom": "examples"}, "name": "worker"},
    "description": "Example worker",
    "bundles": ["FileTools", "Bash"],
    "models": ["Global", {"Fixed": {"model": "openrouter/example/model", "effort": {"Fixed": "High"}}}],
    "default": false,
    "subagents": {"groups": [], "items": []},
    "skills": {"groups": ["Global"], "items": []}
  }
]
```

Model names here are illustrative, not catalogue recommendations. JSON enums
use their Rust variant spellings: `"Global"`, `"Parent"`, and externally tagged
objects such as `{"Custom":"examples"}` or `{"Fixed":"High"}`. API configs
require a nonempty `models`; omitted ordinary fields use their declared defaults
(empty strings/lists, false `selectable`, empty hooks/inclusions, true `default`).
Unlike TOML discovery, omitted API inclusions do not include default groups.

## Discovery and source publication

- Server-global discovery reads `<data_dir>/agents/*.toml` and
  `<data_dir>/skills/<directory>/SKILL.md`. The usual Linux data root is
  `~/.local/share/psa`; it is not a hard-coded library storage location.
- Server startup uses the resolved server `data_dir` (CLI `--data-dir`). Library
  integrations can pass `ConfigRegistry::new(custom_data_dir)` to
  `server::serve_with_configuration(listener, database, providers, probes,
  configuration, shutdown)` to set both discovery and uploaded-skill storage.
  Uploads are stored beneath `skill-uploads` in that root.
- Probe projects use `.agents/{agents,skills}`. At session creation, start at the
  probe cwd and walk upward to the first `.agents` directory; stop there, even
  when empty or missing its `agents`/`skills` subdirectories. Ancestor projects
  are not merged. Retain this selection on the session and rediscover it when
  cwd changes. Paths are normalized absolute paths; probe identity distinguishes
  projects with the same path on different machines.
- There is no probe-global defaults directory or group. `global_defaults`
  includes `ConfigGroup::Global`, not a second set of probe-local defaults.
- Scans validate the complete selected source batch before transactional
  publication. A failed scan must not publish partial changes or treat a
  transient read/probe failure as deletion. Successful scans remove missing
  discovery-owned entries without deleting unrelated API-owned registrations.
- Agent batches support forward references and cycles. Duplicate names within
  one discovery group or duplicate config IDs in one upsert batch are errors;
  upserting an ID from an earlier registration still replaces its current value.

## TOML input and inclusions

`AgentDefinitionConfig` is the loader type, lowered to `AgentConfig`. Its group
comes from discovery and is not repeated in the file. Ordinary agent fields
match API fields. The file-specific inclusion tables are:

```rust
struct SubagentInclusionsConfig {
    global_defaults: bool,
    project_defaults: bool,
    subagents: Vec<String>,
}
struct SkillInclusionsConfig {
    global_defaults: bool,
    project_defaults: bool,
    skills: Vec<String>,
    additional_paths: Vec<PathBuf>,
}
```

- Both inclusion booleans default to `true`, including when their table is
  omitted. Project-wide subagent inclusion excludes the defining agent itself;
  explicit self references and cycles remain supported.
- Included groups are applied in order, with later groups overriding earlier
  groups by name. Explicit items override group selections last, even when an
  explicit item is missing; this prevents silently selecting another source.
- File names resolve first in the defining config group, then globally. The
  selected project group follows Global in default group expansion.
- Resolve `additional_paths` relative to the defining TOML file. Parse each
  directory's `SKILL.md`, use metadata names with directory-name fallback, and
  register it in the defining file's group before resolving references. Repeated
  references to the same skill directory can share that registration; distinct
  directories with the same name in one group are an error.
- Skill metadata is `SKILL.md` frontmatter, not separate skill TOML.

This fixed-model TOML representation is accepted by
`toml::from_str::<AgentDefinitionConfig>` (the same parser used by discovery):

```toml
name = "worker"
models = ["Global", { Fixed = { model = "openrouter/example/model", effort = { Fixed = "High" } } }]
default = false

[subagents]
global_defaults = false
project_defaults = false

[skills]
global_defaults = false
project_defaults = false
```

For session/global inheritance alone, omit `models` or write `models = ["Global"]`.
`models = []` is invalid during discovery. Fixed effort uses the case-sensitive
`Effort` variants (`Off`, `Minimal`, `Low`, `Medium`, `High`, `Xhigh`, `Max`);
inherited effort can instead be `effort = "Parent"` or `effort = "Global"` inside
`Fixed`. Other optional file fields include `description`, `selectable`,
`bundles`, `tools`, `extra_system_prompt`, and `[hooks]`; hooks contain the
`agent_create`, `session_update`, `before_request`, and `after_request` arrays.

## Creation-time model choice

```rust
struct NewConfiguredAgent {
    config_id: ConfigId,
    name: String,
    subagent_parent_id: Option<AgentId>,
    interactive: bool,
    model: Option<String>,
}
```

For example, `POST /sessions/1/agents`:

```json
{
  "config_id": {"config_group": {"Custom": "examples"}, "name": "worker"},
  "name": "worker-1",
  "model": "openrouter/example/model"
}
```

`model` is a choice string, not a `ModelSelection` object: fixed choices use the
exact configured provider/model string; Global and Parent use lowercase
`"global"` and `"parent"`. Omitted/null model selects the sole choice, or the first
choice when `default` is true. Multiple choices with `default = false` require
an explicit selection. `interactive` defaults to false and parent to none.

`Parent` and `Global` model/effort policies resolve at creation time, not as live
bindings. Parent falls back to the session/global model without a parent. The
subagent creation tool applies the same model-choice policy to a permitted
pinned template, without replacing its source graph with a newer mutable config.

## Immutable snapshots and current status

```rust
struct ResolvedAgentConfig {
    config: AgentConfig,
    definition: AgentDefinition,
    references: ResolvedDefinitionReferences,
}
struct ResolvedDefinitionReferences {
    subagents: Vec<AgentDefinitionId>,
    skills: Vec<SkillId>,
}
struct AgentConfigStatus {
    config: AgentConfig,
    status: AgentDefinitionStatus,
}
struct AgentDefinitionStatus { missing_components: Vec<MissingComponent> }
enum MissingComponent {
    Agent { config_id: ConfigId },
    Skill { config_id: ConfigId },
}
```

These fields have distinct purposes:

- `config` preserves source identity, model policies, and inclusion requests.
- `definition` is the existing internal executable snapshot. It contains `id`,
  `previous_version_id`, `version`, `name`, ordinary executable fields,
  `model_config: Option<ModelConfig>`, `skills: Vec<PathBuf>` (materialized
  probe-local `SKILL.md` paths), and
  `permitted_subagents: Vec<AgentDefinitionId>`. Its `skills` are **paths**, not
  skill record IDs; its model is a resolved execution choice, not `models`.
- `references` separately records immutable subagent IDs and mutable skill IDs.
  It must not be confused with source `IncludedItems` or executable skill paths.
- `AgentConfigStatus` is current lookup/list/upsert response metadata, not a
  snapshot identity field or a member of `ResolvedAgentConfig`.

Resolution allocates cyclic graph IDs before filling references, publishes only
complete graphs, and reuses matching snapshots. Changed dependencies produce
new dependent snapshots for new agents, including transitive dependents;
existing agents keep their pinned graph. Skill metadata/content changes alone
need not change a snapshot when its resolved IDs/paths and executable fields
remain the same.

Missing configured agents or skills remain in source configuration, generate
structured warnings, and are omitted from newly resolved references and skill
prompts rather than preventing creation. Restored components can resolve again.
This is not permission to ignore failed scans, cross-probe incompatibility, or
required materialization failures.

## Refresh, materialization, and `SyncDir`

Before configured agent creation, refresh global sources and the session's
retained project. Refresh mutable skill metadata, resolve the reachable graph,
and complete required skill copies before building prompts or exposing the new
agent. `GET /skills` and `GET /agent-configs` use the same refresh pipeline; a
session-scoped list refreshes its selected project as well as globals, while an
unscoped list refreshes globals only. Listing does not copy content to probes.

Server-owned skills materialize to `<probe_data_dir>/skill-cache/<SkillId>`.
Change detection hashes the complete directory's paths, types, modes and file
bytes, not only `SKILL.md` or timestamps. A sibling SHA-256 marker records a
successful sync; an existing unchanged managed copy can be reused. Project
skills stay at their probe-owned source path. Copies never target those sources.

The implemented generic probe `SyncDir` operation shares `UploadDir`'s streamed
uncompressed-tar validation, staging, cancellation and cleanup pipeline:

- The parent must exist. Create a missing destination or replace an existing
  directory with the complete source tree, removing stale files. Reject existing
  file and symlink targets. `UploadDir` remains create-only.
- Stage on the destination filesystem. Publish a missing target with a
  non-replacing rename; atomically exchange an existing directory using Linux
  `renameat2(RENAME_EXCHANGE)`, then clean up the old tree. There is no
  delete-then-rename gap. Failure/cancellation before publication leaves the old
  destination intact.
- Serialize publication to the same destination and report completion only
  after publication. Prompts must not refer to a partially copied directory.
  No incremental/delta transfer or stronger concurrent-reader snapshot guarantee
  is required.

Refresh and sync state are operational bookkeeping, not skill versioning.
Existing saved prompts are not rewritten when skills change, but files at their
managed paths can change.

## Deliberate limits

- Refresh is request-driven, not a background file watcher.
- Skills and managed copies do not preserve historical content. Automatic
  retention/garbage collection of snapshots and unused probe caches is not
  provided by this API. Replacing an API-uploaded skill cleans up its obsolete
  server upload directory.
- A selected unavailable/inaccessible project or failed required copy fails the
  operation rather than silently falling back. Offline session creation or cwd
  changes defer project discovery until the first session-scoped refresh with
  the probe online; a completed discovery finding no project is retained until
  cwd changes, and forks inherit discovery status unless they override cwd.
- Paths are normalized lexically; discovery does not promise cross-symlink
  canonical project identity on remote probes.
- Atomic replacement depends on Linux/filesystem support for directory exchange;
  the implementation does not weaken it to a non-atomic delete-and-rename.
