# Required Features

Http endpoints, with server side events streaming to client. streams in agent events and partial completed events.

## Client authentication

All client HTTP/SSE endpoints, including `GET /health`, require
`Authorization: Bearer <client_key>` using `[psa].client_key` from the server's
`auth.toml`. Missing or invalid credentials return HTTP 401. The separate
`GET /probes/connect` route uses `[psa].probe_key`, not the client key. Keys grant
their respective trusted access; there is no per-user authorization.

Construct the async Rust client with `Client::new(base_url, client_key)`, or
`Client::with_http_client(base_url, client_key, http)` for a custom reqwest
transport. Both constructors require the client key; all requests, including SSE,
send it as a bearer token, overriding any default Authorization header on the
transport. Use TLS or a trusted encrypted tunnel for remote connections.

## Client-safe configuration

Authenticated `GET /config` returns `{"default_agent_name":"root"}` (or the
server-configured name). `Client::get_config()` fetches this suggestion; it does
not guarantee a matching agent configuration exists.

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

`GET /sessions/{session_id}/agents/{agent_id}/status` returns an
`AgentStatusSnapshot` with the effective model configuration, the last inference's
reported total tokens (or the sum when both input and output are reported), and
an inclusive agent event `head`. Token usage is neither cumulative spend nor an
exact current context size; it is unknown after a reset, compaction, failed or
cancelled inference, or missing usage. To reconcile with an already subscribed
stream, apply committed agent events newer than `head` in agent event order;
ignore older replay. The model catalog provides the optional model context limit.

/sessions/{id}/agents/{id}/events
- query events in a range
  - optional flag to include non persisted events
  - `Accept: text/event-stream` for SSE; `Accept: application/json` for JSON

## Ordered session summaries and the global feed

Legacy `GET /sessions?parent=ID` / `Client::list_sessions(parent)` still return
roots (no parent) or direct children in ID order.

`GET /sessions/query` / `Client::query_sessions(&SessionListQuery)` return
`SessionsPage { sessions, next_before, revision }`. Set `flat=true` to include
forks; otherwise `parent` selects children or its absence selects roots.
`flat=true` and `parent` together are invalid. Optional `probe_id` filters the
scope. Summaries include published busy state, ordered by `updated_at DESC, id
DESC`. `limit` defaults to 100 (1–1000); pass opaque `next_before` as `before`.
Cursors are exclusive and bound to the filter scope, not to page size. Keyset
pages are not an immutable snapshot: merge by session ID and reconcile the feed.

`GET /events?kind=session_updated&probe_id=UUID` /
`Client::stream_global_events(&GlobalEventsQuery, last_event_id)` are a separate
SSE-only summary feed. The typed allowlist currently contains only
`GlobalEventKind::SessionUpdated`; no transcript data is exposed. Messages:

- `ready { revision }`: subscription established.
- `session_updated { revision, session }`: full committed summary upsert.
- `resync_required`: discard the bootstrap and start a fresh subscription/reload.

Subscribe first, wait for ready, buffer updates while fetching pages, and apply
updates newer than each page's `revision`. Keep per-session revisions so a late
page cannot overwrite newer state. Revisions are global monotonic publication
order within one server lifetime, not timestamps: busy transitions do not change
`updated_at`. Creation, metadata/model edits, agent creation, event commits, and
busy transitions publish summaries under the same gate. Token/provisional
updates do not publish summaries. Both server and client buffering must be
bounded; the server closes with resync on overflow. EOF/reconnect also requires
fresh bootstrap; supplying Last-Event-ID explicitly returns resync, not replay.
There is no durable replay or deletion operation. Refresh probe/model catalogs
separately.

## Recent and backward transcript history

Both event scopes accept JSON-only `recent=true` for the latest bounded page or
exclusive `before=N` for an older page. Events remain chronological.
`EventsPage.next_before` and `has_older` identify older history; `head` is the
captured live cursor. Agent cursors are event IDs, session cursors are session
positions (including gaps from agent creation). `limit` remains 1–10000.

Connect the existing forward SSE with `after=page.head` and no upper bound;
commits between page capture and SSE connection are replayed, then snapshot
replaces pending events/questions. Recent JSON pages may include current pending
state; older `before` pages always return empty pending/questions and must not
replace current live state. Merge committed events by scoped identity.
`recent` and `before` are mutually exclusive, incompatible with nonzero
`after` or `through`, and rejected on SSE. Existing forward pagination,
Last-Event-ID replay, finite through streams, and provisional promotion remain
unchanged. Empty history still has a usable head cursor.

Transcript `Message` events may include an optional `description` display label,
used for system instructions. It is omitted when absent; older events without it
remain readable. The label is not included in model-facing message content.

## Agent configurations and immutable snapshots

Named configurations are mutable; agent creation resolves and pins an immutable
executable snapshot. A `ConfigId` contains `config_group` and `name`, not a
snapshot ID. The current routes and async client methods are:

- `POST /agent-configs` / `upsert_agent_configs(&configs)` atomically creates or
  replaces a JSON array of `AgentConfigInput` values and returns HTTP 200 with
  `Vec<AgentConfigStatus>`. Omitted/null bundles and omitted hook stages inherit
  server defaults; explicit lists replace them, including empty lists. Passing
  normalized `AgentConfig` values through the client makes all lists explicit.
- `GET /agent-configs?refresh_session_id=ID` /
  `refresh_and_list_agent_configs(refresh_session_id)` refreshes global discovery
  and optionally the selected session's project, then returns all registered
  configurations and dependency statuses. The session ID controls discovery,
  not result filtering.
- `POST /agent-configs/lookup` / `get_agent_config(&config_id)` accepts a JSON
  `ConfigId` and returns its current `AgentConfigStatus`, or HTTP 404 if unknown.
  A status contains the normalized `config` and `status.missing_components`.
- `GET /agent-snapshots/{definition_id}` /
  `get_resolved_agent_config(definition_id)` returns HTTP 200 with a
  `ResolvedAgentConfig`, or JSON `null` (`None`) for an unknown ID. It contains
  captured source `config`, executable `definition`, and resolved `references`
  for subagent snapshots and mutable skill records.

`POST /sessions/{session_id}/agents` accepts `NewConfiguredAgent` with a
`config_id`, instance `name`, and optional allowed `model` selection. It resolves
and pins the current configuration graph; subsequent configuration upserts do
not modify existing agents' snapshots. Legacy `/agent-definitions` CRUD is
removed, not retained as a compatibility API. See
[configuration schema and resolution](../docs/AGENT_DEF_SCHEMA.md) for input
fields, model policies, discovery, and snapshot semantics.

Definitions contain `name`, `description`, `selectable`, `bundles`, `hooks`, an
optional `model_config` override, `skills` paths, and `permitted_subagents` IDs.
Components use `Vec<BundleConfig>` and `HookConfigs` stage lists of `HookConfig`,
not identity enums. Tools are installed only by bundles; there is no standalone
tool configuration or `tools` field on configs, definitions, or defaults.
`ToolId` remains a runtime tool identity.
Hook lists (`agent_create`, `session_update`, `before_request`, `after_request`)
default to empty in normalized definitions. Supported hook configurations are
`Noop`, `StopWithoutTools`, `DefaultStartup`, `ToolDefinitions`, `Cwd`, `Skills`,
`ExtraSystemPrompt`, `ProjectContext`, `SessionModelUpdate`, and
`Compaction { reserve_tokens, keep_recent_tokens, context_window }`. Compaction
runs only in `before_request`; omitted budgets default to 16384/20000 and the
optional context-window override defaults to the model catalog. Generated server
configs enable it. See [compaction](../docs/COMPACTION.md), including hidden
summarizer agents and `Sessions::await_result`. For ordinary
execution, explicitly configure
`agent_create: [DefaultStartup, ToolDefinitions, Cwd, ProjectContext, Skills]` in that order,
`session_update: [SessionModelUpdate]`, and `after_request: [StopWithoutTools]`.
No global hooks are installed by default; explicit bundles may supply their own hooks.

`DefaultStartup` emits `NewContext`, the resolved `Model`, and a base assistant
identity `System` message for root agents, without tools. `ToolDefinitions`
separately emits instance tool-definition events. Both skip forks to preserve
inherited context. `Cwd` emits a `System` message containing
`<cwd>\nSESSION_CWD\n</cwd>` with the session working directory XML-escaped; it
does not skip forks and can also be bound to `session_update`.

`ExtraSystemPrompt { prompt: String }` emits its own configured prompt verbatim
as a separate `System` message, unless empty. It is opt-in with a payload, not an
empty startup hook or an agent-level `extra_system_prompt` field. JSON uses
`{"ExtraSystemPrompt":{"prompt":"Follow the project conventions."}}`; a TOML
stage entry is `{ ExtraSystemPrompt = { prompt = "Follow the project conventions." } }`.
Other unit hook variants remain strings. Like `Cwd`, it does not skip forks;
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
permission boundary. Extra system text comes only from explicitly configured
`ExtraSystemPrompt` hook payloads.

`bundles` is an ordered list of `BundleConfig` values, defaulting to `[]` in both
requests and responses. `Noop` installs the Noop tool.
`{"FileTools":{"read_only":false}}` installs Read, Edit, and Write in that order;
true installs only Read. The flag defaults to false inside the required payload;
bare `"FileTools"` is invalid. TOML uses `{ FileTools = { read_only = true } }`. `Bash` installs `Bash`, `InteractBash`, and `CloseBash`.
`{"Goal":{"interactive_only":true}}` installs goal tools and stopping policy;
`{"AskQuestion":{"interactive_only":true}}` installs only `ask_question`, with no
hooks. Both require payload objects (bare `"Goal"` is invalid). Omitting the flag
inside either payload defaults to true: noninteractive agents get only
`StopWithoutTools` from Goal and nothing from AskQuestion. False fully enables
the bundle on noninteractive agents, including Goal persistence and continuation.
Generated server defaults include Goal with `interactive_only = true`, not AskQuestion.
TOML uses `{ Goal = { interactive_only = true } }` or
`{ AskQuestion = { interactive_only = true } }` in the bundle list.
`Subagents` installs group tools with
session-shared coordination resources. See `../docs/GOALS_AND_SUBAGENTS.md` for
configuration, fixed limits, and persistence/fork behavior. For example:

```json
{"name": "File assistant", "bundles": [{"FileTools": {"read_only": false}}], "hooks": {"agent_create": ["DefaultStartup", "ToolDefinitions"]}}
```

Bundles are installed in list order before explicitly configured hooks.
Tools have no independent configuration path. Duplicate bundle IDs, tool IDs, or
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
`ListAgents`, `AskQuestion`, and `RecordResult`. All tools require their corresponding
bundles; configuring them as standalone tools is rejected. See `../docs/BASH_TOOLS.md` for names, waits, logs,
and environment behavior. Use `{"FileTools":{"read_only":false}}` for full file
operations or `{"FileTools":{"read_only":true}}` for Read alone. Native file
tools operate on the session probe and port Pi's model-facing prompts and
text/image results; they do not add Pi's UI/programmatic result metadata.
`Read` uses Pi's line/byte head truncation and default image limits. See
`../docs/FILE_TOOLS.md` for behavior, safety bounds, and compatibility limitations.

Creating an agent takes `config_id`, `name`, optional `interactive` and `hidden`
(both default `false`), and optional `model: ModelSelection`. For example:

```json
{"config_id":{"config_group":{"Custom":"examples"},"name":"worker"},"name":"Main","interactive":false,"hidden":false,"model":{"Fixed":{"model":"openrouter/vendor/model","effort":{"Fixed":"High"}}}}
```

The model selector must exactly match an allowed entry, including its effort
policy. Other selectors are `"Global"` and `"Parent"`; fixed effort policies can
be `"Global"` or `"Parent"` only when configured exactly that way. Omitted/null
selects the sole entry, or the first when `default` is true. Registration/upsert
normalizes stored `AgentConfig.default` to true for single-model configurations;
multiple entries with `default = false` require explicit selection.
Native `create_agent` uses the same
typed selector, and its allowed-template identity prompt includes full selectors.

Agent responses include the exact definition ID and `interactive` and `hidden`
flags. `hidden` is client visibility metadata only: the server stores and returns
it without filtering agents, and forks inherit it. The `interactive` flag
is creation-time metadata available to tools and hooks, not an automatic UI or
permission mode. Explicit Goal and AskQuestion bundles use it only when their
`interactive_only` construction flag is true; false enables them on any agent. Forked agents inherit it. `CreateSession.model_config`
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
`through`, `recent`, `before`, `limit`, `include_pending`, `results_only`, and
`from_context_start`; it has no `stream` field.
`results_only=true` filters durable Result events before the database limit and
filters SSE durable events as well. `from_context_start=true` (agent routes,
JSON only, with `through` and no other lower bound) begins the page at the model
context containing `through`: its opening `NewContext` event, or the agent's
first event. Continue a partial page with `after = next_after`. Event endpoints
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
Agent views include state and cumulative token usage. `cache_write_tokens` counts
reported cache writes (currently OpenRouter chat `prompt_tokens_details.cache_write_tokens`);
it is null when any included call did not report a count. Totals include fork-inherited
calls and compactions, so summing across forks double-counts inherited usage.
See `../runtime/psa_server/README.md` for startup, `auth.toml`, client examples,
queue/shutdown semantics, and the trusted-client-only security boundary.

## Client queries

Client queries are ephemeral runtime requests for input, not transcript events.
They do not require `interactive: true`, a UI, or a currently connected client.
Outstanding queries are included as `queries: [ClientQueryRequest, ...]` in
forward/recent JSON event pages and SSE `snapshot` messages on the existing
session/agent event endpoints, regardless of `include_pending`. Backward history
pages requested with `before` return empty queries; merging history must not
clear current query state. Payloads from older API versions without this field
deserialize with an empty list. Replace local query state for the stream's scope
when receiving a snapshot; upsert by query ID on
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
restart or copied by session forks. There is no separate query stream. The opt-in
`AskQuestion { interactive_only }` bundle exposes a native `ask_question` tool
using this API; its installation policy does not gate low-level query calls.

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
optional `label`, absolute `data_dir`, optional absolute `home_dir`, boolean `host`),
timestamps, and live `is_active`.
`Client::list_probes` and `Client::get_probe` wrap these endpoints.

Probes authenticate `GET /probes/connect` with `Authorization: Bearer <probe_key>`.
Query parameters are `version=2` and the metadata fields above; success upgrades
to WebSocket. Invalid auth returns 401, incompatible versions/metadata return
400, and an already-connected UUID returns 409. Offline records persist.

`home_dir` is advertised from the probe process's `HOME` when it is an absolute
UTF-8 path of at most 4096 bytes; otherwise it is omitted. Legacy metadata may
omit it. It is a hint, not a guarantee that the directory exists or is accessible.

`host` defaults to false for legacy metadata. A probe reads it from root
`host = true` in the server's `config.toml` beside its selected `probe.toml`.
At most one registered probe may be host; reconnecting the same UUID is allowed.
The flag asserts a shared server filesystem: server-owned skills use their
original paths without cache synchronization on host probes. Remote probes
continue receiving synchronized skill copies.

`GET /probes/{probe_id}/directories?path=...` lists immediate children of an
absolute UTF-8 path on a connected probe. The client-authenticated endpoint is
read-only and returns an array of `DirectoryEntry` objects (`name`, `kind`,
`size`); kinds are `file`, `directory`, `symlink`, or `other`. Entry metadata
does not follow symlinks. Paths are passed unchanged, not canonicalized, and
results are not guaranteed sorted. `Client::list_probe_dir(id, path)` safely
encodes the query. A malformed path returns 400 `invalid_input`, an unknown
probe 404 `not_found`, a disconnected or overloaded probe 503 `unavailable`,
and a remote filesystem or protocol failure 502 `probe_error`. The probe's
filesystem access is not restricted to the session workspace: client keys are
trusted credentials.

Session creation requires `probe_id`, referencing a registered probe. It is
returned in session responses, remains fixed, and is inherited on fork.

## Durable results

`EventType::Result(AgentResult::Text(String) | AgentResult::Json(Value))` stores a
completion artifact, excluded from model history. Completion policies produce
at most one per run. Failure/cancellation does not synthesize one; a database
save already accepted can still finish after cancellation. Completion modes are
ordinary `StopWithoutTools` (text), Goal (text only once inactive), or
`TypedResult { schema: ToolSchema, max_retries }` (JSON). They are mutually
incompatible, including inherited default hooks. No policy is allowed, but provides
no automatic completion or Result. Interactive-only Goal falls back to
`StopWithoutTools` on noninteractive agents. Token-limited ordinary text can complete; running Goal continuation never
treats a token limit as completion. Non-running Goal states include Blocked: the
goal remains retained/paused while the current run settles Idle with a text Result.

TypedResult requires an explicit object-root Draft 2020-12 schema with local
references, validated offline by shared `CompiledToolSchema`. It installs
sequential `record_result` and a require-result hook. Whole tool arguments are
the result, without a wrapper. Validation success atomically saves Result and
Idle, acknowledging only `Result recorded.`; invalid data is a tool error and
missing normal completion reprompts. Three retries are allowed by default after
the initial violation; the fourth interrupts without a fake result. Zero fails
immediately. The budget resets only at a new non-Running to Running transition,
not midrun steering. Recovery after a committed artifact but before its tool
acknowledgment restores only the producing call acknowledgment, without another
Result or an Interrupted status. `BundleConfig::typed_result::<T>()` and
`typed_result_with_retries::<T>(n)` generate typed configuration.

`GET /sessions/{sid}/agents/{aid}/results?limit=1` returns `Vec<ResultRecord>`
newest first across the full logical fork history, respecting fork checkpoints.
`ResultRecord<T = AgentResult>` contains `session_id`, `agent_id`, `event_id`,
`source_agent_id`, `position`, `timestamp`, and `value`. The source ID is physical
storage provenance only. `AgentResult::decode::<T>()` decodes the payload, and
record `decode::<T>()` preserves the metadata. Event consumers can also use
`EventEnvelope::into_result_record()` or `EventEnvelope::decode_result::<T>()`.
Text decodes as a string, not
parsed JSON source. Client methods are `results`, `last_result_raw`,
`last_result::<T>`, and `last_results::<T>`; typed mismatches return errors.
See [durable result examples and semantics](../docs/AGENT_RESULTS.md).

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

`PUT /providers/credential` takes `{"provider":"Openrouter","credential":"..."}`
to set or replace a credential; `credential: null` removes it. Both fields are
required. The response is a `ProviderInfo`, never the credential. The typed client
method is `set_provider_credential(&SetProviderCredential)`. Changes are applied
to future inference and catalog status immediately; in-flight inference keeps its
existing provider. External file edits are not automatically reloaded; restart or
another successful credential update loads them. Credentials are not
validated against the provider and token refresh is not implemented. The existing
client key is full-trust: any client with it can change provider credentials and
incur spending. Protect traffic with a trusted encrypted tunnel.

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
