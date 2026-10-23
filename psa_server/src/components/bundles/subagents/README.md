# Native Subagents port

The session owns one coordinator actor; each agent bundle contains only its
cloneable handle. Children are fresh non-interactive agents in the same PSA
session. Coordinator parent/group relationships never use `Agent.parent_id`
(which remains fork lineage). Names are session-unique, including non-group
agents. Explicit template names resolve only against the caller's pinned
`permitted_subagents` definition IDs. There is deliberately no implicit first
permitted template/default.

Fixed first-port policy matches Pi defaults: at most eight active workers and
four child nesting levels (root depth zero). Interactive roots do not consume
worker slots; waiting workers do. The actor reserves capacity before child
creation and idle-worker resume, so parallel calls cannot oversubscribe slots.
Capacity exhaustion rejects the operation instead of queuing it. Custom limits
and workflow/group nesting configuration are not implemented.

All mutable membership, waiters, reservations and logical-run deduplication live
in the actor. Session-manager operations run as coordinator-owned futures, not
inside mailbox handlers. Shutdown drops pending futures immediately and aborts
on a bounded fallback; no child tasks are detached. Cancelled startup reconciles
a committed child via interrupt rather than attempting deletion.

Message bodies are delivered once through attributed steering, which resumes
an idle agent. An active wait can return acknowledgement metadata (`id` and
`from`) after steering is accepted. As a PSA adaptation, wait_message otherwise
returns `steering_available` immediately when steering is available, without
consuming it or waiting for unrelated group-delivery jobs. Unlike Pi's
historical acknowledgement inbox, acknowledgements are not retained for future
waits: consumed old steering must not spuriously wake a later wait. Deadlock
evaluation is deferred and counts pending create/send jobs as progress,
including sibling calls in a parallel batch.

Automatic child reports do not steer an interrupted or failed parent back into
a run. Child status/error remains visible in list_agents; explicit send_message
can resume a stopped parent. Ordinary idle parents still receive background
completion reports. Started lifecycle events, not stale invocation snapshots,
are authoritative when promoting a known terminal member to running.

## Persistence and fork policy

Names/IDs, parents, groups and template aliases are checkpointed in caller
transcripts using namespaced `psa.subagents/v1:` JSON Log events. The identity
hook also checkpoints members before their requests. First use in a new runtime
reads member traces through `Sessions.load_agent_events`; there are no direct DB
accesses, new tables or persistence protocols. Recovered membership is reconciled
against current session agent rows: nonexistent IDs are dropped, Idle/Running
rows clear stale errors, and Interrupted rows preserve checkpointed failure
diagnostics only when the checkpoint identifies an error. Other Interrupted rows
remain interrupted, preventing child reports from waking stopped parents after
restart. Lifecycle events received during restore override the older row snapshot:
latest Finished outcomes remain terminal, and a newer Started event wins as
running. Physical event
ownership excludes inherited fork logs,
so a fork starts its own group rather than retaining the original group IDs.

This is **not a durable messaging service**: steering delivery, outstanding waits,
reservations, in-flight jobs and run-report deduplication are runtime-only.
Checkpointing and agent creation/steering are not one atomic transaction. A
server crash or forced invocation cancellation between creation and checkpoint
can leave a durable agent without recovered group membership. Such records are
not deleted or silently assigned to another group; their names remain reserved.
Lifecycle reports accepted by a live coordinator are deduplicated by logical
run ID, but replay/exactly-once reporting across server restart is not promised.
Runtime shutdown cancels pending coordinator work and does not await RPCs back
to the shutting-down session manager.
