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

Definitions contain `name`, `description`, `selectable`, `bundles`, `tools`, `hooks`, an
optional `model_config` override, `skills` paths, `permitted_subagents` IDs, and
`extra_system_prompt`. Hook lists (`agent_create`, `session_update`,
`before_request`, `after_request`) default to empty. Supported hook IDs are
`Noop`, `StopWithoutTools`, `DefaultStartup`, `ToolDefinitions`, `Cwd`, `Skills`,
`ExtraSystemPrompt`, `ProjectContext`, and `SessionModelUpdate`. For ordinary
execution, explicitly configure
`agent_create: [DefaultStartup, ExtraSystemPrompt, ToolDefinitions, Cwd, ProjectContext, Skills]` in that order,
`session_update: [SessionModelUpdate]`, and `after_request: [StopWithoutTools]`.
No global hooks are installed by default; explicit bundles may supply their own hooks.

`DefaultStartup` emits `NewContext`, the resolved `Model`, and a base assistant
identity `System` message for root agents, without tools. `ToolDefinitions`
separately emits instance tool-definition events. Both skip forks to preserve
inherited context. `Cwd` emits a `System` message containing
`<cwd>\nSESSION_CWD\n</cwd>` with the session working directory XML-escaped; it
does not skip forks and can also be bound to `session_update`.

`ExtraSystemPrompt` emits the pinned definition's `extra_system_prompt` verbatim
as a separate `System` message, unless empty. Like `Cwd`, it does not skip forks;
configure it only at stages where another copy of the prompt is wanted.

`ProjectContext` discovers instruction files on the session's probe, from the
filesystem root through the lexically normalized absolute `working_directory`.
In each directory the first readable candidate wins, in this order:
`AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`.
Directories and other non-file entries are skipped; symlinks are read through.
An empty file still wins. Files are combined in ancestor-first order into one
`System` message with Pi-style `<project_context>` and
`<project_instructions path="...">` wrappers. Text is preserved verbatim except
for stripping a leading UTF-8 BOM. No files means no system message.

The hook rereads files on each configured invocation, including forks; it has no
implicit installation, caching, descendant discovery, global Pi agent-directory
loading, or Git-worktree special handling. Configure it on `agent_create` after
`DefaultStartup`; optionally also on `session_update` to append refreshed context.
Repeated invocations append rather than replace previous instructions. Remote
file/listing failures, invalid UTF-8, and size limits emit warning `Log` events;
disconnection and protocol failures propagate. Files are limited to 1 MiB each
and 4 MiB combined, with oversized files skipped. Cancellation drops the active
remote operation without emitting a partial prompt.

Only the configured `SkillsHook` consumes `AgentDefinition.skills`. It reads
explicit locations on the session probe, resolving relative paths against the
session `working_directory`; locations accept `.md` files or directories with
`SKILL.md`. The emitted pi-style catalogue contains XML-escaped names,
descriptions, and paths, never skill bodies. `disable-model-invocation: true`
skips a skill; the first duplicate name wins. Missing/malformed skills emit
warning `Log` events and are skipped. An empty catalogue emits no system message;
a nonempty skills list with a disconnected probe errors. `selectable` remains
selection metadata. `permitted_subagents` is preserved literally and is enforced
by the Subagents tool when creating children, not as a global trusted-client API
permission boundary. `extra_system_prompt` is injected only by the explicitly
configured `ExtraSystemPrompt` hook.

`bundles` is an ordered list of `BundleId` values, defaulting to `[]` in both
requests and responses. `FileTools` installs `Read`, `Edit`, and `Write` in that
order. `Bash` installs `Bash`, `InteractBash`, and `CloseBash`. `Goal` installs
interactive goal tools and stopping policy; `Subagents` installs group tools with
session-shared coordination resources. See `../docs/GOALS_AND_SUBAGENTS.md` for
configuration, fixed limits, and persistence/fork behavior. For example:

```json
{"name": "File assistant", "bundles": ["FileTools"], "hooks": {"agent_create": ["DefaultStartup", "ToolDefinitions"]}}
```

Bundles are installed in list order before explicitly configured tools and hooks;
explicit `tools` and `hooks` remain supported. Duplicate bundle IDs, tool IDs, or
tool names across bundle and explicit installations are rejected at component
installation, not normalized or silently deduplicated. The session manager keeps
installed owners/components by agent ID across idle worker retirement and worker
failures. Forked agents have independent owners. Installed tool/hook instances
stay agent-local; Subagents shares only its coordinator resources within a
session. Shutdown interrupts workers and closes query/session-shared services,
then waits for workers and cleans retained agent bundles in reverse installation
order with timeouts and error reporting. Probe connections
remain available during cleanup. Resources are not persisted across server restart. Definition updates are immutable full replacements: omitting
`bundles` clears the list in the new version without changing the old version or
agents pinned to it. Migration 0009 gives existing definitions an empty list.

Supported tool IDs are `Noop`, `Read`, `Edit`, `Write`, `Bash`, `InteractBash`,
`CloseBash`, `SetGoal`, `SetGoalStatus`, `CreateAgent`, `SendMessage`, `WaitMessage`,
and `ListAgents`. Bash, Goal, and Subagents IDs require their corresponding
bundles; configuring them as standalone tools is rejected. See `../docs/BASH_TOOLS.md` for names, waits, logs,
and environment behavior. Alternatively add independent file tools
explicitly, for example `"tools": ["Read", "Edit", "Write"]`, without also
selecting `FileTools`. Native file
tools operate on the session probe and port Pi's model-facing prompts and
text/image results; they do not add Pi's UI/programmatic result metadata.
`Read` uses Pi's line/byte head truncation and default image limits. See
`../docs/FILE_TOOLS.md` for behavior, safety bounds, and compatibility limitations.

Creating an agent takes `definition_id`, `name`, and optional `interactive`
(default `false` when omitted from JSON). For example:

```json
{"definition_id": 1, "name": "Main", "interactive": false}
```

Agent responses include the exact definition ID and `interactive` flag. The flag
is creation-time metadata available to tools and hooks, not an automatic UI or
permission mode. The explicitly selected Goal bundle uses it to enable goal tools
and its stopping policy. Forked agents inherit it. Creating a session requires `model_config`;
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

## Client queries

Client queries are ephemeral runtime requests for input, not transcript events.
They do not require `interactive: true`, a UI, or a currently connected client.
Outstanding queries are included as `queries: [ClientQueryRequest, ...]` in both
JSON event pages and SSE `snapshot` messages on the existing session/agent event
endpoints, regardless of `include_pending`. Older pages/snapshots without this
field deserialize with an empty list. Replace local query state for the stream's
scope when receiving a snapshot; upsert by query ID on
`client_query_requested { query }` and remove on
`client_query_removed { session_id, agent_id, query_id }`. Query notifications do
not advance the durable event cursor. Reconnecting to the event stream recovers
outstanding requests even when no client was present when they were created.

`ClientQueryRequest` contains UUID `id`, `session_id`, `agent_id`, and `payload`:

```json
{
  "id": "ebbc9c2e-cb57-4c33-aa7b-e3c70f68ae26",
  "session_id": 1,
  "agent_id": 2,
  "payload": {
    "type": "ask_question",
    "payload": {"questions": [{"question": "Which direction?", "choices": ["Left", "Right"]}]}
  }
}
```

Post a `ClientQueryResponse` to
`/sessions/{session_id}/agents/{agent_id}/queries/{query_id}/response`:

```json
{"type": "ask_question", "payload": {"answers": [{"answer": "Straight ahead", "notes": "An optional explanation"}]}}
```

Success returns HTTP 204 with no body. The async client exposes
`respond_to_client_query(session, agent, query_id, &response)`; the route builder
is `routes::client_query_response_path`. Requests need at least one question;
question text and any supplied choices must not be blank. Answers must match
questions positionally and exactly in count, and each answer must be nonblank.
Choices are suggestions: arbitrary freeform/Other answers are valid. `choices`
defaults to `[]`, and `notes` defaults to `None`. Unknown fields are rejected.
Validation errors omit question/answer text.

Errors use the usual `ApiError`: invalid input/response is 400 `invalid_input`,
unknown or incorrectly scoped query is 404 `not_found`, already answered is 409
`already_resolved`, cancelled is 410 `cancelled`, and unavailable/overloaded is
503 `unavailable`. Queries are runtime-only and are not restored after a server
restart or copied by session forks. There is no separate query stream or native
AskQuestion tool implied by this API.

First valid response wins; invalid responses leave the query pending. Cancelling
or dropping the calling future removes the pending query and notifies clients.
Disconnecting a client does not cancel it. No connected client or `interactive`
flag is required, and there is no automatic answer deadline. Runtime shutdown
closes the query service and wakes outstanding callers. The registry accepts up
to 1,024 pending queries and remembers only the last 1,024 resolved/cancelled IDs;
after eviction, later responses receive 404 instead of 409/410. See
`../docs/CLIENT_QUERIES.md` for server-side usage and lifecycle details.

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
