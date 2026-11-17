# Durable agent results

`EventType::Result(AgentResult)` records at most one completion artifact per run:
`AgentResult::Text(String)` or `AgentResult::Json(serde_json::Value)`. Results are
persisted metadata, not conversation messages, and are excluded from model
history. Failure or cancellation does not synthesize a result. Cancellation
before a result commit produces none, but an accepted database save cannot be
revoked by cancellation and may still complete.

## Completion modes

Configure at most one completion mode. No-policy configurations are allowed,
but provide no automatic completion or Result:

- `StopWithoutTools`: ordinary no-tool completion records the response text and
  settles Idle. Token-limited text can be a result, including
  `IncompleteTokenLimit` output.
- `Goal { interactive_only }`: while a goal is active, keep continuing even at a
  token limit; no completion result is recorded. Inactive means
  `!GoalState::is_running()`: completed, unset, and Blocked all qualify. A blocked
  goal remains retained/paused, while the current run settles Idle with a text
  Result; completion does not clear that blocked goal. With `interactive_only = true`,
  noninteractive agents install only `StopWithoutTools`, without Goal tools or continuation.
- `TypedResult { schema, max_retries }`: require the schema-validated
  `record_result` tool to complete with a JSON result.

These modes are mutually incompatible, including completion hooks inherited from
server defaults. A Goal or TypedResult configuration must not also select
`StopWithoutTools`. Override an inherited `after_request` list with `[]` when the
bundle supplies the completion hook. Generated server defaults use Goal with
`interactive_only = true` instead of an explicit `StopWithoutTools` hook.

## Typed JSON results

```json
{
  "bundles": [
    {"TypedResult": {
      "schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
      },
      "max_retries": 3
    }}
  ],
  "hooks": {"after_request": []}
}
```

The canonical schema field is a raw `ToolSchema`, not a string or a result
wrapper. It requires an explicit object root, JSON Schema Draft 2020-12, and
local references only. The server's `ResultSchema` validation is offline;
remote schema resolution is not supported.

The bundle installs a sequential `record_result` tool and a require-result
completion hook. The tool's entire argument object is the result: for the schema
above, call it with `{"answer":"done"}`, not `{"result":{"answer":"done"}}`.
A valid invocation atomically saves the JSON Result and Idle state. Its tool
acknowledgment is only `Result recorded.`; it does not echo the result payload.
Invalid data produces a tool error and no Result. A normal terminal response
without the required tool is not accepted as completion; the hook reprompts.

`max_retries` defaults to 3 when omitted: three correction retries after the
initial invalid/missing completion, with the fourth violation interrupting the
run with a reason rather than inventing a Result. Zero interrupts on the first
violation. The budget resets only for a new run (a non-Running to Running
transition), not for steering received during an existing run.

If recovery sees a checkpoint after the typed artifact was committed but before
its tool acknowledgment, it restores only the producing call's acknowledgment.
It neither duplicates the Result nor marks that accepted completion Interrupted.
An accepted database save may finish despite later cancellation; this is not a
rollback guarantee.

Rust callers can generate configuration with
`BundleConfig::typed_result::<T>()` (three retries) or
`BundleConfig::typed_result_with_retries::<T>(n)`. `T` supplies its schema through
`schemars::JsonSchema`; schema and completion-mode validation still occur on the
server.

## Retrieval and typed decoding

`GET /sessions/{session_id}/agents/{agent_id}/results?limit=1` returns a vector of
results newest first. The limit defaults to one. Retrieval includes the full
logical fork history, respecting each fork checkpoint, rather than only events
physically stored under the viewed agent.

`psa::api::results` exposes `ResultRecord<T = AgentResult>`, retaining:

- `session_id` and `agent_id`: the viewed session and agent.
- `event_id`: the event sequence in that agent's logical lineage.
- `source_agent_id`: the physical event owner, possibly a fork ancestor; this is
  storage provenance, not a restriction on visible history.
- `position`: the session-wide event position.
- `timestamp`: the event time, serialized as RFC 3339.
- `value`: the raw tagged result or decoded payload.

`AgentResult::decode::<T>()` decodes the inner value; the record's
`decode::<T>()` also preserves its metadata. Text is decoded as a string, not
parsed as JSON source. Type mismatches return errors rather than skipping a
result or substituting another artifact. Event-stream consumers can also use
`EventEnvelope::into_result_record()` and `EventEnvelope::decode_result::<T>()`
to obtain result records while retaining envelope provenance.

Client methods:

- `list_results(session, agent, limit)`: tagged result records, newest first.
- `get_last_result(session, agent)`: newest record, or None.
- `get_last_result_as::<T>(session, agent)`: newest decoded record, or None.
- `list_results_as::<T>(session, agent, limit)`: decoded records newest first.

`EventsQuery.results_only = true` filters durable events to Results before the
database limit is applied; SSE also filters durable event delivery. It does not
change result provenance or turn artifacts into model history. Event endpoints
still select JSON versus SSE with the Accept header, not a stream query flag.

## Tool definitions

The static identity lookup `tool_definition(ToolId)` returns
`Option<ToolDefinition>`: `RecordResult` has no universal static schema and
returns None. The installed tool instance carries the exact configured schema;
use its definition when exposing tools to a model.
