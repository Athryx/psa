# Agent configuration and definition snapshots

## Implemented API and identity

The configuration API is implemented in `lib/psa/src/api/configs.rs` and `lib/psa/src/api/definitions.rs`, with HTTP
routes in `runtime/psa_server/src/http.rs` and a thin `psa::api::Client`. Discovery is in
`runtime/psa_server/src/config_registry`; registration and immutable graph resolution are
in `runtime/psa_server/src/db/configurations.rs`.

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
| `POST /agent-configs` | JSON array of `AgentConfigInput` | `Vec<AgentConfigStatus>` | `upsert_agent_configs(&[I])` (`I: Clone + Into<AgentConfigInput>`) |
| `GET /agent-configs` | none | `Vec<AgentConfigStatus>` | `list_agent_configs()` |
| `POST /agent-configs/lookup` | `ConfigId` JSON body | `AgentConfigStatus` | `get_agent_config(&ConfigId)` |
| `GET /agent-snapshots/{definition_id}` | Internal snapshot ID | `ResolvedAgentConfig` (404 if unknown) | `get_resolved_agent_config(AgentDefinitionId)` |
| `POST /skills` | `NewSkill` JSON body | `Skill` (201) | `upload_skill(&NewSkill)` |
| `GET /skills` | none | `Vec<Skill>` | `list_skills()` |
| `POST /configs/refresh` | `RefreshConfigs { session_ids }` | 204 | `refresh_configs(&[SessionId])` |
| `POST /sessions/{session_id}/agents` | `CreateAgent` | `Agent` (201) | `create_agent(SessionId, &CreateAgent)` |

Lookup by compound config ID uses a body rather than path encoding. An unknown
snapshot returns JSON `null`; a missing current config produces a not-found
error. Listing never rescans; it returns every registered group. `POST
/configs/refresh` rescans the global directory and the `.agents` directory of
each listed session (discovering it on first use), for agent configurations and
skills together. Snapshot reads and current-config lookup do not perform
discovery.

There is **no backwards compatibility**: `/agent-definitions` CRUD routes,
constants, and client methods are removed. The old `CreateAgent` body is not
accepted by the agent-creation HTTP route. Internal pinned-definition creation
remains available to the subagent runtime, not as an alternate HTTP API.

## Current configuration and skill types

`AgentConfigInput` is omission-aware registration input; `AgentConfig` is the
normalized stored/returned form. The client accepts slices of either type.
The following field summaries use the actual Rust API names:

```rust
struct ConfigId { config_group: ConfigGroup, name: String }
enum ConfigGroup {
    Custom(String),
    Global,
    Project { path: String, probe_id: ProbeId },
}
struct IncludedItems { groups: Vec<ConfigGroup>, items: Vec<ConfigId> }

// Normalized form: all component lists are explicit.
struct AgentConfig {
    config_id: ConfigId,
    description: String,
    selectable: bool,
    bundles: Vec<BundleConfig>,
    hooks: HookConfigs,
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

enum BundleConfig {
    Noop, Bash, Subagents,
    FileTools { read_only: bool },
    Goal { interactive_only: bool },
    AskQuestion { interactive_only: bool },
    TypedResult { schema: ToolSchema, max_retries: u32 },
}
enum HookConfig {
    Noop, StopWithoutTools, DefaultStartup, ToolDefinitions, Cwd, Skills,
    ExtraSystemPrompt { prompt: String }, ProjectContext, SessionModelUpdate,
    Compaction { reserve_tokens: u64, keep_recent_tokens: u64, context_window: Option<u64> },
}
struct HookConfigs {
    agent_create: Vec<HookConfig>,
    session_update: Vec<HookConfig>,
    before_request: Vec<HookConfig>,
    after_request: Vec<HookConfig>,
}

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

### Component construction configurations

All tools are installed by bundles: there is no standalone tool configuration
or `tools` field on agent configs, stored definitions, or server defaults.
`ToolId` remains a tool identity enum, not a construction selector.
`Noop` is a unit bundle installing the Noop tool. `FileTools { read_only }`
requires a payload: `{"FileTools":{"read_only":true}}` installs only Read;
false or omission inside the payload installs Read, Edit, and Write.
Bare `"FileTools"` is invalid. TOML uses
`{ FileTools = { read_only = true } }` in the bundle array.
`BundleConfig` also selects `Bash`, `Subagents`,
`Goal { interactive_only }`, and `AskQuestion { interactive_only }`.
The latter two require payload objects: `{"Goal":{"interactive_only":true}}` or
`{"AskQuestion":{"interactive_only":true}}`. An omitted flag inside either payload
defaults to true (for example `{"Goal":{}}`); bare `"Goal"` is invalid. TOML uses
`{ Goal = { interactive_only = true } }` or
`{ AskQuestion = { interactive_only = true } }` in the bundle array.
With true, noninteractive agents get only `StopWithoutTools` from Goal and nothing
from AskQuestion; false fully enables the bundle regardless of the agent flag.
Generated server defaults include Goal with `interactive_only = true`, not AskQuestion.
`AskQuestion` installs only its tool, with no hook. `Goal` retains its goal state,
persistence, and continuation policy even for noninteractive agents when false.
`TypedResult { schema, max_retries }` installs a sequential JSON result tool and
completion hook. The schema is a raw, explicit object-root `ToolSchema` (Draft
2020-12, local references only); retries default to three. It is incompatible
with Goal and `StopWithoutTools`, including default-inherited hooks. Override
`hooks.after_request` with `[]` to disable an inherited ordinary completion hook.
See [durable results](AGENT_RESULTS.md) for validation, retry limits, typed
configuration helpers, and retrieval.
`DecisionModels { include_global, models }` installs the help-only CodeMode
functions `list_decision_models()` and `await decide(model, state, questions)`.
`models` lists `provider/model` slugs the agent may call; `include_global`
(default false) also allows the session's decision model and makes it the default
when `model` is omitted. Without it, `model` may be omitted only when exactly one
model is legal. For example,
`{"DecisionModels":{"include_global":true,"models":["openrouter/typesafe/jev-1.14"]}}`.
Each call is recorded as a public `DecisionCall` event holding the complete
request (including `state`), the provider response or failure message, and the
duration. A session runs at most
`[decisions] max_concurrent_calls` (server `config.toml`, default 8) calls at
once; further calls queue until a slot frees.

`HookConfigs` contains four ordered `Vec<HookConfig>` stages. Unit hook and bundle
variants remain JSON/TOML strings. The `ExtraSystemPrompt` hook instead carries
its own prompt; there is no agent/config `extra_system_prompt` field:

```json
{"hooks":{"agent_create":["DefaultStartup",{"ExtraSystemPrompt":{"prompt":"Follow the project conventions."}},"ToolDefinitions"]}}
```

The equivalent file hook stage is:

```toml
[hooks]
agent_create = ["DefaultStartup", { ExtraSystemPrompt = { prompt = "Follow the project conventions." } }, "ToolDefinitions"]
```

`Compaction {}` runs only in `before_request`, defaulting to a 16384-token
reserve and 20000 recent tokens. Generated server defaults enable it; see
[compaction](COMPACTION.md) for hidden summarizer agents and replay semantics.

Prompt hooks are opt-in; ordinary startup lists omit `ExtraSystemPrompt` unless
an explicit prompt payload is provided. Definitions, defaults, API input, and
TOML discovery all store construction configurations rather than identity IDs.

### Batch JSON example

`POST /agent-configs` takes an array of `AgentConfigInput`, even for one config:

```json
[
  {
    "config_id": {"config_group": {"Custom": "examples"}, "name": "worker"},
    "description": "Example worker",
    "bundles": [{"FileTools": {"read_only": false}}, "Bash"],
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
(empty strings/inclusions, false `selectable`, true `default`). Components follow
the server defaults below. Unlike TOML discovery, omitted API inclusions do not
include default groups.

### Server component defaults

The server's `config.toml` can supply fallbacks shared by TOML files and API input:

```toml
[agent_defaults]
bundles = [{ FileTools = { read_only = false } }, "Bash", "Subagents"]

[agent_defaults.hooks]
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
after_request = ["StopWithoutTools"]
```

An omitted `bundles` field inherits its entire configured default
list; an explicit list replaces it, and `[]` disables it. Hook stages inherit
independently: omitted `hooks.after_request` inherits, while
`hooks.after_request = []` disables that stage without affecting other stages.
An omitted `[agent_defaults]` means all fallback lists are empty. Defaults are
not appended to explicit lists.

`AgentConfigInput` represents `bundles` as `Option<Vec<BundleConfig>>` and `hooks` as
`HookOverrides` whose four stages are `Option<Vec<HookConfig>>`.
`AgentConfigInput::new(config_id, models)` leaves components omitted, inheriting
server defaults. Converting a normalized `AgentConfig` into input makes every
component list explicit, including empty lists; resubmitting a returned config
therefore does not unexpectedly inherit defaults.

Defaults resolve at API registration or file refresh, not at agent execution.
Changing `config.toml` requires a server restart; there is no live config watch.
API callers must resubmit omission-aware input to adopt changed defaults, while
file refresh resolves omitted fields again. Existing snapshots never change.

## Discovery and source publication

- Server-global discovery reads `<data_dir>/agents/*.toml` and
  `<data_dir>/skills/<directory>/SKILL.md`. The usual Linux data root is
  `~/.local/share/psa`; it is not a hard-coded library storage location.
- Server startup uses the resolved server `data_dir` (CLI `--data-dir`). Library
  integrations can pass `ConfigRegistry::new(custom_data_dir)` to
  `server::serve_with_configuration(listener, database, providers, probes,
  configuration, default_model_config, shutdown)` to set both discovery and
  uploaded-skill storage. `default_model_config: ModelConfig` supplies the concrete
  session creation fallback; CLI resolution exposes it as
  `ResolvedConfig.session_defaults`.
  Use `ConfigRegistry::new(custom_data_dir).with_agent_defaults(agent_defaults)`
  to configure library-side component defaults. Uploads are stored beneath
  `skill-uploads` in that root.
- Probe-local configuration lives in `.agents/{agents,skills}`, registered as
  `ConfigGroup::AgentsDir { path, probe_id }`. At session creation, start at the
  probe cwd and walk upward to the first `.agents` directory; stop there, even
  when empty or missing its `agents`/`skills` subdirectories. Ancestor
  directories are not merged. Retain this selection on the session and
  rediscover it when cwd changes. Paths are normalized absolute paths; probe
  identity distinguishes directories with the same path on different machines.
  This is unrelated to a session's `project_id` label.
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
match API input, including omission-aware components and per-stage hooks; both
loaders normalize against the same server defaults. The file-specific inclusion
tables are:

```rust
struct SubagentInclusionsConfig {
    global_defaults: bool,
    group_defaults: bool,
    subagents: Vec<String>,
}
struct SkillInclusionsConfig {
    global_defaults: bool,
    group_defaults: bool,
    skills: Vec<String>,
    additional_paths: Vec<PathBuf>,
}
```

- Both inclusion booleans default to `true`, including when their table is
  omitted. `global_defaults` includes `ConfigGroup::Global`; `group_defaults`
  includes the defining config's own group (for a global config, Global again).
  Inclusion never reaches across groups, so a global config does not include a
  session's `.agents` items. Group-wide subagent inclusion excludes the defining
  agent itself; explicit self references and cycles remain supported.
- Included groups are applied in order, with later groups overriding earlier
  groups by name. Explicit items override group selections last, even when an
  explicit item is missing; this prevents silently selecting another source.
- File names resolve first in the defining config group, then globally. The
  own group follows Global in default group expansion.
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
group_defaults = false

[skills]
global_defaults = false
group_defaults = false
```

For session/global inheritance alone, omit `models` or write `models = ["Global"]`.
`models = []` is invalid during discovery. Fixed effort uses the case-sensitive
`Effort` variants (`Off`, `Minimal`, `Low`, `Medium`, `High`, `Xhigh`, `Max`);
inherited effort can instead be `effort = "Parent"` or `effort = "Global"` inside
`Fixed`. Other optional file fields include `description`, `selectable`,
`bundles` and `[hooks]`; hooks contain the
`agent_create`, `session_update`, `before_request`, and `after_request` arrays.
Omitted component fields inherit `[agent_defaults]`; for example, `bundles = []`
and `[hooks]` with `after_request = []` explicitly disable those defaults.

## Creation-time model choice

```rust
struct CreateAgent {
    config_id: ConfigId,
    name: String,
    subagent_parent_id: Option<AgentId>,
    interactive: bool,
    hidden: bool,
    model: Option<ModelSelection>,
}
```

For example, `POST /sessions/1/agents`:

```json
{
  "config_id": {"config_group": {"Custom": "examples"}, "name": "worker"},
  "name": "worker-1",
  "model": {"Fixed": {"model": "openrouter/example/model", "effort": {"Fixed": "High"}}}
}
```

`model` is a typed `ModelSelection`: `"Global"`, `"Parent"`, or a `Fixed` object.
The complete selector must exactly match a configured entry, including the fixed
provider/model string and effort policy. Fixed effort policies may also be
`"Parent"` or `"Global"`, but only when that exact policy is configured; selecting
an equivalent resolved effort is not sufficient. Omitted/null model selects the
sole choice, or the first choice when `default` is true. Database registration
and upsert normalize stored `AgentConfig.default` to true for single-model
configurations, even when input specifies false. Multiple choices with
`default = false` require explicit selection. `interactive` and `hidden` default
to false, and parent to none. `hidden` is client visibility metadata only: the
server stores and returns it without filtering agents. Forked agents inherit it.

`Parent` and `Global` model/effort policies resolve at creation time, not as live
bindings. Parent falls back to the session/global model without a parent. The
subagent creation tool applies the same typed selector and exact-match policy to
a permitted pinned template, without replacing its source graph with a newer
mutable config. Its allowed-template identity prompt carries full model selectors,
not just provider/model names.

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
  `ModelConfig` contains a `model` slug (such as `openrouter/vendor/model`) and
  `effort`.
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
retained `.agents` directory. Refresh mutable skill metadata, resolve the
reachable graph, and complete required skill copies before building prompts or
exposing the new agent. `POST /configs/refresh` uses the same refresh pipeline
for globals plus each listed session's directory; it does not copy content to
probes. `GET /skills` and `GET /agent-configs` only read the registry.

For remote probes, server-owned skills materialize to
`<probe_data_dir>/skill-cache/<SkillId>`. A probe advertising `host: true` shares
the server filesystem and instead uses the original server-owned `SKILL.md`
paths, without synchronization, hashing, cache writes, or source replacement.
For remote copies, change detection hashes the complete directory's paths, types, modes and file
bytes, not only `SKILL.md` or timestamps. A sibling SHA-256 marker records a
successful sync; an existing unchanged managed copy can be reused. `.agents`
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
- A selected unavailable/inaccessible `.agents` directory or failed required
  copy fails the operation rather than silently falling back. Offline session
  creation or cwd changes defer discovery until the first refresh naming the
  session with the probe online; a completed discovery finding no directory is
  retained until cwd changes, and forks inherit discovery status unless they
  override cwd.
- Paths are normalized lexically; discovery does not promise cross-symlink
  canonical directory identity on remote probes.
- Atomic replacement depends on Linux/filesystem support for directory exchange;
  the implementation does not weaken it to a non-atomic delete-and-rename.
