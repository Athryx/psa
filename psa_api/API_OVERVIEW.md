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
  - `Accept: text/event-stream` for SSE; `Accept: application/json` for JSON

/sessions/{id}/agents/{id}
- create agent
- list agents
- get agents
- submit requests to either interrupt or send steering message

/sessions/{id}/agents/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - `Accept: text/event-stream` for SSE; `Accept: application/json` for JSON

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

Creating an agent takes `config_id`, `name`, optional `interactive` (default
`false`), and optional `model: ModelSelection`. For example:

```json
{"config_id":{"config_group":{"Custom":"examples"},"name":"worker"},"name":"Main","interactive":false,"model":{"Fixed":{"model":"openrouter/vendor/model","effort":{"Fixed":"High"}}}}
```

The model selector must exactly match an allowed entry, including its effort
policy. Other selectors are `"Global"` and `"Parent"`; fixed effort policies can
be `"Global"` or `"Parent"` only when configured exactly that way. Omitted/null
selects the sole entry, or the first when `default` is true. Registration/upsert
normalizes stored `AgentConfig.default` to true for single-model configurations;
multiple entries with `default = false` require explicit selection.
Native `create_agent` uses the same
typed selector, and its allowed-template identity prompt includes full selectors.

Agent responses include the exact definition ID and `interactive` flag. The flag
is creation-time metadata available to tools and hooks, not an automatic UI or
permission mode. The explicitly selected Goal bundle uses it to enable goal tools
and its stopping policy. Forked agents inherit it. `CreateSession.model_config`
is optional: omission or null uses server `session_defaults`, and an explicit
`ModelConfig` overrides that default. Session responses always include the concrete
persisted configuration; session updates can replace it explicitly. Reloads,
updates, and forks do not reapply creation defaults.

`ForkedSession.agents` is a flat `Vec<Agent>`; each returned agent's `parent_id`
is the source agent ID, rather than a separate source/child tuple.

TODO: other stuff from client like configs, mcp, etc
TODO: probe api

## Implementation

Exact route templates/builders are in `src/routes.rs`; wire types are in
`src/types.rs` and `src/events.rs`. `src/client.rs` provides the async `Client`,
including typed JSON requests and SSE decoding. `EventsQuery` uses `after`,
`through`, `limit`, and `include_pending`; it has no `stream` field. Event endpoints
select SSE with `Accept: text/event-stream`, or JSON with
`Accept: application/json` (JSON is the default). `Client::events()` requests JSON;
`Client::stream_events()` requests SSE. Only the streaming method accepts a
`Last-Event-ID` resume cursor. SSE supports provisional-to-persistent promotion.

Session views include runtime `busy`; `SessionUpdated` publishes a full `Session`
snapshot for both metadata edits and busy-state changes. There is no separate
`SessionBusy` message. Metadata writes and the supervisor's published activity
projection share the publication gate, preventing stale busy values from
overwriting newer transitions. Apply snapshots in stream order; activity-only
changes do not advance the persisted `updated_at` timestamp.
Agent views include state and cumulative token usage. See `../psa_server/README.md` for startup, `auth.toml`, client examples,
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

## Providers and models

`Model` identifies a model by `provider: ProviderType` and `api_name: String`.
The API name is provider-specific (for example `vendor/model` on OpenRouter),
without a harness provider prefix. Rust callers construct it with
`Model::new(ProviderType::Openrouter, "vendor/model")?`.
`ModelConfig` adds `effort` to that identity; `provider` is nested inside `model`,
not a top-level configuration field. For example, a session's `model_config` is:

```json
{"model":{"provider":"Openrouter","api_name":"vendor/model"},"effort":"Medium"}
```

Model configuration files instead use a provider/API-name slug string, never a
structured `Model`. Server creation defaults in `config.toml` are:

```toml
[session_defaults]
model = "openai_codex/gpt-5"
effort = "Medium"
```

For OpenRouter, use a slug such as `openrouter/vendor/model`. If the whole
`[session_defaults]` section is absent, the built-in default is OpenAI Codex
`gpt-5` with `Medium` effort. A present section requires both fields. Resolved
server configuration exposes this as `ResolvedConfig.session_defaults: ModelConfig`.

`GET /providers` returns all providers PSA supports (`Openrouter` and
`OpenAiCodex`), with `id` and `authed`. Display names are available locally through
`ProviderType::name()`. Authentication means a
credential is configured in the running server; it does not verify token validity,
model entitlement, or account balance. Credentials never appear in responses.

`GET /models` returns a `ModelInfo[]`. Optional `provider`, `authed`, and `pinned`
query parameters combine with AND. Omitted filters include both values; for
example `/models?provider=Openrouter&authed=true&pinned=true` lists pinned
OpenRouter models when OpenRouter credentials are configured. `authed=false`
selects models whose provider has no configured credentials.

Each entry contains `model: { provider, api_name }`, display `name`,
`input_modalities`, optional `context_window` and `max_output_tokens`, `cost`,
`reasoning`, `reasoning_options`, live `authed`, persisted `pinned`, and RFC 3339
`refreshed_at`. Cost values are reference USD per million tokens: `input`,
`output`, `cache_read`, and `cache_write`; null means unknown, and zero means
free in the upstream catalog. Optional pricing tiers contain `input_tokens_above`
and the rates applicable above that threshold. These are not guaranteed charges
or subscription pricing. `reasoning_options` lists selectable `effort` values
and their resolved `wire_effort`; null wire effort omits reasoning parameters.

`GET /models/lookup?provider=Openrouter&model=vendor%2Fmodel` returns one entry,
or 404 when unknown. Model names are query values so slash-containing IDs work.
`Client::model_info(&Model)` performs this lookup using the model's provider
and API name; reasoning effort is not part of model identity. `ModelLookup`
keeps flat `provider` and `model` query fields, with `model` holding the API name.

`PUT /models/pin` accepts:

```json
{"model":{"provider":"Openrouter","api_name":"vendor/model"},"pinned":true}
```

`ModelInfo` and `PinModel` carry the provider only inside `model`, not as a
separate top-level field. The pin endpoint returns the updated entry.
Pins are server-wide and survive restarts and
catalog refreshes. Pinning an unknown model returns 404. Unpinning is idempotent;
it returns JSON null if that model has disappeared. Models removed upstream no
longer appear in lists, but their saved pins apply if they reappear later.

`POST /models/resync` accepts `{}` for all providers or
`{"provider":"Openrouter"}` for one. It bypasses the freshness interval, waits
for every requested provider's network revalidation, and returns HTTP 204 with
no body only if all succeed. If any refresh fails, it returns HTTP 502 with
`ApiError.code = "catalog_sync_failed"`; successful provider updates are still
retained. The Rust client returns `Result<(), ClientError>`, without a provider
list or individual result entries. Concurrent scheduled/manual refreshes share
each provider's in-flight request, and disconnecting a caller does not cancel
shared work.

The server fetches Pi's public chat catalogs on missing/stale startup and every
four hours, using ETags. Successful snapshots replace a provider's catalog
atomically. A 304 refreshes its timestamp without replacing metadata. Failures
preserve the last good catalog. No model data is bundled: a new database returns
an empty list until its first successful sync. Reads and inference never fetch
remote metadata. The Rust client exposes `list_providers`, `list_models`,
`model_info`, `set_model_pinned`, and `resync_models`.

`Effort` now accepts `Off` and `Minimal` alongside `Low`, `Medium`, `High`,
`Xhigh`, and `Max`. Before inference, known model metadata resolves mappings
(such as Minimal to Low) and rejects unsupported choices with the supported
settings. There is no silent clamping. The configured effort stays unchanged;
the resolved setting is stored in `InferenceResultStart.request_config.reasoning`
as `Omit` or `{"Effort":"Low"}`. Off is encoded as `none` on the provider wire.
For unknown models, existing inference behavior and protocol effort defaults
remain available. No token verification, login, or token refresh is added.
