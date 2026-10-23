# Subagents bundle

Subagent ancestry is core agent metadata. `Agent.subagent_parent_id` records an
immutable parent in the same session and is saved atomically with agent creation.
`Agent.parent_id` continues to mean fork lineage. The create/get/list API and
agent snapshot events expose ancestry without requiring this bundle. Session
forks remap subagent links to copied agents and clear links to absent parents.
Existing agents default to no subagent parent; legacy `psa.subagents/v1:` logs
remain historical events and are neither read nor written for coordination.

The session is the interaction boundary. `list_agents` lists direct children by
default and the whole session with `all=true`. `send_message` targets any other
agent in the session by unambiguous name. Child creation requires an explicit
name from the caller's pinned `permitted_subagents` definitions. Names must be
nonblank and session-unique at child creation; existing API-created root agents
may have duplicate names, in which case name-based messaging rejects ambiguity.
There are no group IDs, fixed child concurrency/depth limits, or interrupt cascades.

Messages use `AgentRequest::SubagentMessage` with ordinary steering semantics.
Optional source metadata contains agent ID/name; the server validates supplied
IDs against the session and resolves names from core state. Persisted messages
have `MessageType::Subagent`. Projection renders them as attributed user-role
context, preserving structured source metadata in transcript/API events. Initial
child tasks use the same path. Pending requests remain runtime state, with the
same crash and cancellation guarantees as ordinary steering.

`wait_message` wraps core `Sessions::wait_for_steering` without consuming queued
messages. It returns `steering_available` or `timeout`, with no acknowledgement
IDs. The timeout defaults to 120 seconds and must be positive and at most 600.
The small session-shared bundle actor tracks logical active runs, guarded wait
registrations, and pending create/send operations. It rejects waits when no
session run can progress outside a wait, deferring briefly and while operations
or completion callbacks are pending. Runs without the bundle count as progress.
Cancellation, dropped futures, timeouts, and shutdown release registrations.

A bundle lifecycle callback runs once per logical run, independent of worker
retirement. The completion hook reports a child's answer/failure/interruption
when its parent has the Subagents bundle; the child need not have that bundle.
Reports are ordinary attributed steering with a 30,000-byte middle-truncated
body. Background submission checks the parent's stopped state in the manager,
and checks again before consumption, so it cannot resume an interrupted/failed
parent. Explicit messages may resume it. Completion callbacks are best effort,
not durably replayed after crashes.

Lifecycle hooks enqueue work outside the supervisor's command loop. Session
shutdown cancels callback jobs and waits without awaiting RPCs back to the
shutting-down manager. Bundle handles hold no duplicate hierarchy or status.
