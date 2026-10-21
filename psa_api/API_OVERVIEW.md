# Required Features

Http endpoints, with server side events streaming to client. streams in agent events and partial completed events.

## Session and Agent

/sessions/{id}
- create session
- fork session
- update session
- view session
- list sessions

/sessions/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - flag to enable sse streaming

/sessions/{id}/agents/{id}
- create agent
- list agents
- get agents
- submit requests to either interrupt or send steering message

/sessions/{id}/agents/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - flag to enable sse streaming

## Agent definitions

`POST /agent-definitions` creates a reusable definition; `GET /agent-definitions`
lists definitions. `GET /agent-definitions/{definition_id}` retrieves an exact
immutable version. `PUT /agent-definitions/{definition_id}` accepts a full
replacement and returns a new ID with `previous_version_id` pointing to the
original version and an incremented `version`. Both writes return HTTP 201.
The async client exposes matching `create_agent_definition`,
`list_agent_definitions`, `get_agent_definition`, and `update_agent_definition`
methods.

Definitions contain `name`, `description`, `selectable`, `tools`, `hooks`, an
optional `model_config` override, `skills` paths, `permitted_subagents` IDs, and
`extra_system_prompt`. Hook lists (`agent_create`, `session_update`,
`before_request`, `after_request`) default to empty. Supported hook IDs are
`Noop`, `StopWithoutTools`, `DefaultStartup`, `ToolDefinitions`, `Cwd`, `Skills`,
`ExtraSystemPrompt`, and `SessionModelUpdate`. For ordinary execution, explicitly configure
`agent_create: [DefaultStartup, ExtraSystemPrompt, ToolDefinitions, Cwd, Skills]` in that order,
`session_update: [SessionModelUpdate]`, and `after_request: [StopWithoutTools]`.
No hooks are installed implicitly.

`DefaultStartup` emits `NewContext`, the resolved `Model`, and a base assistant
identity `System` message for root agents, without tools. `ToolDefinitions`
separately emits instance tool-definition events. Both skip forks to preserve
inherited context. `Cwd` emits a `System` message containing
`<cwd>\nSESSION_CWD\n</cwd>` with the session working directory XML-escaped; it
does not skip forks and can also be bound to `session_update`.

`ExtraSystemPrompt` emits the pinned definition's `extra_system_prompt` verbatim
as a separate `System` message, unless empty. Like `Cwd`, it does not skip forks;
configure it only at stages where another copy of the prompt is wanted.

Only the configured `SkillsHook` consumes `AgentDefinition.skills`. It reads
explicit locations on the session probe, resolving relative paths against the
session `working_directory`; locations accept `.md` files or directories with
`SKILL.md`. The emitted pi-style catalogue contains XML-escaped names,
descriptions, and paths, never skill bodies. `disable-model-invocation: true`
skips a skill; the first duplicate name wins. Missing/malformed skills emit
warning `Log` events and are skipped. An empty catalogue emits no system message;
a nonempty skills list with a disconnected probe errors. `selectable`,
and `permitted_subagents` remain literal metadata, not automatic selection or
permission enforcement. `extra_system_prompt` is injected only by the explicitly
configured `ExtraSystemPrompt` hook.

Creating an agent takes `definition_id`, `name`, and optional `interactive`
(default `false` when omitted from JSON). For example:

```json
{"definition_id": 1, "name": "Main", "interactive": false}
```

Agent responses include the exact definition ID and `interactive` flag. The flag
is creation-time metadata available to tools and hooks; it does not change runtime
behavior. Forked agents inherit it. Creating a session requires `model_config`;
session responses include it and session updates can replace it.

TODO: other stuff from client like configs, mcp, etc
TODO: probe api

## Implementation

Exact route templates/builders are in `src/routes.rs`; wire types are in
`src/types.rs` and `src/events.rs`. `src/client.rs` provides the async `Client`,
including typed JSON requests and SSE decoding. Event queries use `after`,
`through`, `limit`, `include_pending`, and `stream` parameters. SSE supports
scoped `Last-Event-ID` resume cursors and provisional-to-persistent promotion.

Session views include runtime `busy`; agent views include state and cumulative
token usage. See `../psa_server/README.md` for startup, `auth.toml`, client examples,
queue/shutdown semantics, and the trusted-client-only security boundary.

## Probes

`GET /probes` lists registered systems; `GET /probes/{probe_id}` fetches one UUID.
Both return flattened probe metadata (`id`, `hostname`, `os`, `architecture`,
optional `label`, absolute `data_dir`), timestamps, and live `is_active`.
`Client::list_probes` and `Client::get_probe` wrap these endpoints.

Probes authenticate `GET /probes/connect` with `Authorization: Bearer <probe_key>`.
Query parameters are `version=1` and the metadata fields above; success upgrades
to WebSocket. Invalid auth returns 401, incompatible versions/metadata return
400, and an already-connected UUID returns 409. Offline records persist.

Session creation requires `probe_id`, referencing a registered probe. It is
returned in session responses, remains fixed, and is inherited on fork. No
remote-operation HTTP endpoints are exposed yet: use server-side probe handles.
