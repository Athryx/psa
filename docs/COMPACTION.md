# Automatic context compaction

The `Compaction` hook runs in `before_request`, after the preceding tool batch has
finished. Generated server configs enable it; existing configs are not rewritten.

```toml
[hooks]
before_request = [{ Compaction = { reserve_tokens = 16384, keep_recent_tokens = 20000 } }]
```

JSON uses `{"Compaction":{}}` with the same defaults. `context_window` optionally
overrides the model catalog's context limit; without a known limit the hook does
nothing. An empty `before_request` list disables the default. Compaction is not
a completion mode and works with Goal, StopWithoutTools, and TypedResult.

## Preparation

The implementation follows Pi's compaction prompts and conversation rendering:
`packages/coding-agent/src/core/compaction/{compaction,utils}.ts` in
`earendil-works/pi` (MIT). Prompts are kept in
`runtime/psa_server/src/components/hooks/compaction/prompts.rs`.

The trigger is `context_tokens > context_window - reserve_tokens`. Accounting uses
the latest valid response's usage plus estimated trailing messages, falling back
to character estimates when usage is unavailable. Cached and reasoning counts are
already subsets in PSA and are not added twice. Reset contexts do not reuse old
usage measurements.

Cuts walk backwards to the recent-token budget, choosing user or assistant
boundaries, never separating a tool call from its results. A split user-message
span produces a separate prefix checkpoint, merged with the history summary.
Repeated compactions update the previous summary with the newly discarded span.

Summarizer input is labeled text, not executable conversation history. System
prompts are excluded, thinking is omitted, tool calls are rendered with arguments,
and tool results are truncated to 2000 UTF-16 units with Pi's truncation marker.
Read/modified file lists accumulate across checkpoints.

## Summarizer agents and persistence

Each standalone summary request creates an ordinary, noninteractive agent with
`hidden = true`, no tools, no compaction hook, the source agent's active model,
and Pi's summarization system prompt. Split spans may need two requests. A retained
hook counter names them `_internal_<agent_name>_compaction<count>`; it resumes above
existing matching names when reconstructed. Summary agents have no subagent parent,
so completion does not inject an unsolicited subagent message into the source.
The cached summarizer definition is nonselectable.

`Sessions::await_result(session, agent, after, cancellation)` waits for a durable
result using publication notifications. An optional event boundary excludes older
results. Interruption/failure without a result, cancellation, and runtime shutdown
release the waiter. A newly created idle agent may still start after submission;
initial idleness alone does not terminate the wait.

Only a nonempty, completed text summary is accepted. The parent is updated in one
batch: `NewContext`, model, copied system prompts and tool schemas, `Compaction`,
and the retained conversation. Retained model transitions preserve conversion
semantics. Old raw history stays available for audit, forks, and results; run
status and Goal diagnostics are not replaced. Inference delimiters are not copied
into the retained tail, preventing stale usage from triggering another compaction.
Summarization usage is billed on the hidden agent, not duplicated on the parent.

Cancellation interrupts the active summarizer. Failure never commits a partial
replacement, and follows the ordinary hook-failure path for the parent's run.
This implements threshold compaction, not manual compaction or provider-overflow
retry recovery. Provider-native compacted contexts are not converted into lossy
text summaries. Summary output uses the model's ordinary output limit, matching
current Pi rather than imposing the older reserve-derived summary cap.

`hidden` is stored and returned by the server without filtering agent lists or
streams. Clients decide whether to display hidden agents; normal UIs should hide
them by default.
