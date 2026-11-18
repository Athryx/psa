# Configuration reference

Layout of every PSA config file. Unless noted, files are TOML and unknown keys
are rejected (`deny_unknown_fields`). Files live in the application data
directory (`--data-dir`, default is the platform's local data dir for `psa`).

## Server (`config.toml`)

Path: `<data_dir>/config.toml` (override with `psa_server start --config`).
Generated with defaults on first start if missing (along with the default
`agents/root.toml` and `agents/subagent.toml`); an existing file is never
rewritten. Credentials live beside it in `auth.toml` (see the end of this
section).

```toml
# Marks this machine as the server's host. Probes read this marker to know
# they run on the same machine as the server. Default: false.
host = true

# Name of the agent configuration suggested for new sessions. Default: "root".
default_agent_name = "root"

# Required.
[server]
# Address the HTTP/SSE API listens on. Required. Overridden by `--listen`.
listen = "127.0.0.1:8080"
# Start an in-process local probe together with the server. Default: false.
autostart_local_probe = true

# Required.
[database]
# SQLite database file, relative to the data directory. Must not be empty.
path = "psa.sqlite3"

# Optional section.
[sandboxes]
# Docker image for sandboxes created without an explicit image.
# Blank/omitted means no default image.
default_image = "ubuntu:24.04"

# Optional section.
[decisions]
# Decision-model calls a session runs at once; further calls queue. Must be
# a positive integer. Default: built-in limit (8).
max_concurrent_calls = 8

# Model used when session creation omits `model_config`. Optional; when the
# section is present both fields are required.
[session_defaults]
# Provider-prefixed model slug: `openai_codex/<model>` or `openrouter/<vendor>/<model>`.
model = "openai_codex/gpt-5"
# One of: "Off", "Minimal", "Low", "Medium", "High", "Xhigh", "Max".
effort = "Medium"

# Fallbacks for omitted fields in agent files and API registrations.
# An explicit list (even an empty one) in an agent replaces the default;
# lists are never merged. Optional section; both lists default to empty.
[agent_defaults]
# Ordered bundle configurations. See the Agent section for all bundles.
bundles = [
    { FileTools = { read_only = false } },
    "Bash",
    "Subagents",
    { Goal = { interactive_only = true } },
    { Compaction = {} },
]

# Per-stage hook defaults; each stage inherits independently and defaults to empty.
[agent_defaults.hooks]
# Run once when an agent is first created.
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
# Run on session updates, including while the agent is idle.
session_update = ["SessionModelUpdate"]
# Run before every inference request.
before_request = []
# Run after every inference request.
after_request = []
```

`auth.toml` (same directory as `config.toml`) holds secrets and is not part of
`config.toml`:

```toml
[psa]
# Key clients (psa library, GUI) present to the server.
client_key = "..."
# Key probes present when connecting. Probes copy this into their own auth.toml.
probe_key = "..."

# Provider credentials, each section optional.
[openai_codex]
access_token = "..."

[openrouter]
api_key = "..."
```

## Probe (`probe.toml`)

Path: `<data_dir>/probe.toml` (override with `psa_probe start --config`). A
missing file is created on start; `id` is generated and written back, and
`--server` / `--label` are persisted into it. The probe also reads
`auth.toml` (`[psa].probe_key`, as above) and the `host` marker of
`config.toml` from the same directory (or beside the server config when run
by the server).

```toml
# Stable probe identity (UUID). Generated automatically if missing; do not
# change it for an existing probe.
id = "00000000-0000-0000-0000-000000000000"

# Server to connect to: an HTTP(S) or WS(S) URL without credentials, query,
# or fragment. Required (set it here or with `--server`).
server_url = "https://psa.example.com"

# Human-readable name shown in clients. Optional; overridden by `--label`.
label = "build-box"

# Allow the server to create Docker sandboxes on this machine. Default: false.
supports_sandboxes = false

# Static probe executable mounted into sandboxes. Optional; defaults to the
# running executable when that is `psa_probe*`. Required with
# `supports_sandboxes = true` when the probe is embedded in another binary
# (such as the server's local probe). Must be UTF-8 without commas.
sandbox_probe_path = "/opt/psa/psa_probe"
```

## Agent (`<root>/agents/<name>.toml`)

One file per agent configuration, discovered in:

- the **global** root: `<data_dir>/agents/*.toml`
- any **`.agents`** directory: `<dir>/.agents/agents/*.toml`, found by walking
  up from a session's working directory on its probe (the nearest one wins)

Anything missing from the file is filled from `[agent_defaults]` in the server
config (`bundles` and each hook stage inherit independently; an explicit
empty list disables the default).

```toml
# Agent name, unique within its group. Required.
name = "root"

# Human-readable description. Default: "".
description = "General-purpose assistant"

# Offer this agent for selection when creating sessions. Default: false.
# Selection metadata only; it does not install tools or hooks.
selectable = true

# Ordered allowed model policies (non-empty). Default: ["Global"].
#   "Global"  follow the session's model and effort
#   "Parent"  fixed to the parent agent's policy when created (root: session)
#   { Fixed = { model = "<slug>", effort = ... } }  pin a provider model, where
#     effort is { Fixed = "High" } (Off/Minimal/Low/Medium/High/Xhigh/Max),
#     "Parent", or "Global"
models = ["Global", { Fixed = { model = "openrouter/vendor/model", effort = { Fixed = "High" } } }]

# Use the first model when no choice is supplied. Default: true (forced to
# true when there is only one model). Formerly `default`, still accepted.
first_model_default = true

# Bundles to install, in order. Omit to inherit server defaults; [] for none.
bundles = [{ FileTools = { read_only = false } }, "Bash", "Subagents"]

# Hook lists per lifecycle stage. Omitted stages inherit server defaults;
# [] disables that stage. Hooks run in listed order.
[hooks]
# Run once when the agent is first created (not on worker reload).
agent_create = ["DefaultStartup", "ToolDefinitions", "Skills"]
# Run on session updates, including while idle.
session_update = ["SessionModelUpdate"]
# Run before each inference request.
before_request = []
# Run after each inference request.
after_request = []
# Parameterized hooks use a table: { ExtraSystemPrompt = { prompt = "Be concise." } }

# Which agents this agent may create as subagents. Name resolution: this
# agent's own group first, then Global. Omitted section: both defaults true.
[subagents]
# Include every agent in the Global group. Default: true.
global_defaults = true
# Include every agent in this agent's own group. Default: true.
group_defaults = true
# Additional named agents to include. Default: [].
subagents = ["subagent"]

# Which skills this agent can load. Omitted section: both defaults true.
[skills]
# Include every skill in the Global group. Default: true.
global_defaults = true
# Include every skill in this agent's own group. Default: true.
group_defaults = true
# Additional named skills to include. Default: [].
skills = ["code-review"]
# Extra skill directories (each containing a `SKILL.md`) registered into this
# agent's group and included. Default: [].
additional_paths = ["/path/to/my-skill"]
```

### Bundles

Bundles install tools (in list order, before hooks) and own their shared
state. Unit bundles are written as a string (`"Bash"`); bundles with fields as
a table (`{ Goal = { interactive_only = true } }`).

- `Noop` — installs a tool that does nothing.
- `FileTools { read_only = false }` — Read, plus Edit and Write unless `read_only = true`.
- `Bash` — Bash, InteractBash, and CloseBash with shared process sessions.
- `CodeMode` — CodeMode execution and reset tools.
- `Goal { interactive_only = true }` — goal tools and goal-aware stopping; with `interactive_only` (default true), noninteractive agents only get StopWithoutTools.
- `AskQuestion { interactive_only = true }` — tool for requesting structured client input; skipped for noninteractive agents when `interactive_only` (default true).
- `Subagents` — session-wide agent tools (create, send message, wait, list).
- `TypedResult { schema, max_retries = 3 }` — schema-validated result recording and completion policy; `schema` is an object-root JSON Schema (required), `max_retries` is the correction retry budget.
- `Traces` — tools for listing psa sessions and exporting them as traces.
- `DecisionModels { models = [], first_model_default = true }` — CodeMode functions for calling decision models. `models` lists the allowed models in order: `{ Fixed = "provider/model" }`, `"Global"` (the session's decision model, if set), or `"Pinned"` (every pinned decision model, in catalog order). It is resolved at each call; a repeated model keeps its first position. With `first_model_default`, a call that names no model uses the first allowed one; without it, calls must name one.
- `Compaction { reserve_tokens = 16384, keep_recent_tokens = 20000, context_window }` — summarizes older context near the context window and handles the `Compact` command; `context_window` overrides the catalog value (optional).

### Hooks

Unit hooks are strings; `ExtraSystemPrompt` is a table.

- `Noop` — does nothing.
- `StopWithoutTools` — stops after a response with no tool calls, unless goal policy applies.
- `DefaultStartup` — initializes a new root agent's model and system context.
- `ExtraSystemPrompt { prompt }` — appends a system message with the given text.
- `ProjectContext` — loads project instructions from the working directory hierarchy.
- `ToolDefinitions` — publishes the installed tools' model-facing definitions.
- `Cwd` — adds the session working directory to context.
- `Skills` — loads the configured skill catalogue.
- `SessionModelUpdate` — follows session model updates unless the agent pins its own model.

## Skill (`<root>/skills/<dir>/SKILL.md`)

Skills are directories containing a root `SKILL.md`, discovered under the same
roots as agents (`<data_dir>/skills/*/` and `<dir>/.agents/skills/*/`), or
named via an agent's `skills.additional_paths`, or uploaded through the API
(tar archive with a root `SKILL.md`). Support files may sit beside it. This
file is Markdown with **YAML** frontmatter, not TOML, so the fields are shown
in a `yaml` block:

```yaml
---
# Skill name; must match the registered config name. Optional: defaults to
# the skill directory's name.
name: code-review

# What the skill does and when to use it. Required, non-empty; shown to the
# model in the skill catalogue.
description: Review a diff for correctness bugs.

# Hide from the model-facing catalogue (so it can only be included
# explicitly). Optional boolean, default: false.
disable-model-invocation: false
---

Markdown body: the instructions loaded when the skill is used.
```

## Desktop (`desktop.toml`)

Path: `<data_dir>/desktop.toml`. Written by the desktop app itself (atomically);
you normally edit settings in the UI. Missing file means defaults; a malformed
file is never overwritten automatically. All keys are optional (`serde(default)`)
and unknown keys are ignored. Window layout and tab state under `workspace` are
managed by the app and not meant to be hand-edited.

```toml
# Id of the profile in use. Falls back to "local" if it does not exist.
active_profile_id = "local"

# Global (not per-server) UI preferences.
[preferences]
# Show agents flagged hidden in the session browser. Default: false.
show_hidden_agents = false
# Transcript font size in px. Default: 14.
transcript_font_size = 14
# Maximum conversation column width in `ch`. Default: 85.
conversation_max_width_ch = 85
# Keep the transcript scrolled to the newest content. Default: true.
auto_follow = true
# Double-clicking a session opens its details. Default: true.
double_click_session_opens_details = true
# Hide results that are only recorded as text. Default: true.
hide_text_recorded_results = true
# Hide agent status lines in transcripts. Default: true.
hide_agent_statuses = true
# Marker at the start of each model turn: "Hidden", "Line", or "Labeled"
# (hairline with turn number, usage and context link). Default: "Labeled".
turn_dividers = "Labeled"
# Reasoning text display: "Hidden", "Collapsed", or "Shown". Default: "Collapsed".
reasoning_display = "Collapsed"

# One entry per server connection. A "local" profile always exists (it is
# re-added if missing); its client key is never written to disk.
[[profiles]]
# Profile identifier ("local" for the built-in one, otherwise a generated UUID).
id = "local"
# Display name.
name = "Local"

[profiles.connection]
# Server API base URL. Default: "http://127.0.0.1:8080".
api_url = "http://127.0.0.1:8080"
# Client key from the server's auth.toml `[psa].client_key`. Stored only for
# remote profiles; never logged.
client_key = ""

# Per-server saved UI state.
[profiles.workspace]
# Server URL this workspace belongs to (informational).
server_url = ""
# Agents last viewed, oldest first; one entry per session.
last_viewed_agents = []

[profiles.workspace.sidebar]
# Sidebar is collapsed. Default: false.
collapsed = false
# Panel shown when expanded: "Browser", "Details", or "Probes". Default: "Browser".
expanded_mode = "Browser"
# Width in CSS px, clamped to 180..=720. Default: 286.
width = 286

[profiles.workspace.session_filter]
# Show only sessions on these probes; empty shows all.
probe_ids = []
# Also include sessions not belonging to any project.
include_global = false
# Show only sessions in these projects; empty shows all.
project_ids = []

# `[profiles.workspace.workspace]` (split/tab layout) is app-managed state.
```
