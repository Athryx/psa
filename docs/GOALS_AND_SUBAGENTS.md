# Goal mode and subagents

Both features are opt-in Rust bundles. Tool/hook instances remain agent-local;
subagent coordination resources are shared by agents within the same PSA session.
Bash processes and goal owners are not accidentally shared between agents.

## Configuration

Create a pinned worker definition first, then include its ID in the parent
`permitted_subagents` list. Example definitions, with the actual returned worker
ID substituted for `42`:

```json
{
  "name": "worker",
  "bundles": ["FileTools", "Bash", "Subagents"],
  "hooks": {
    "agent_create": ["DefaultStartup", "ToolDefinitions", "ExtraSystemPrompt"],
    "after_request": ["StopWithoutTools"]
  },
  "extra_system_prompt": "Complete the assigned task and report to your parent."
}
```

```json
{
  "name": "coordinator",
  "bundles": ["FileTools", "Bash", "Goal", "Subagents"],
  "permitted_subagents": [42],
  "hooks": {
    "agent_create": ["DefaultStartup", "ToolDefinitions"],
    "after_request": ["StopWithoutTools"]
  }
}
```

Create the coordinator agent with `interactive: true` to enable Goal tools.
Subagent tools are independent of this flag. `create_agent` is installed only
when the definition permits at least one child template. Definitions without
permitted children still get messaging, waiting, and listing tools.

Use explicit `agent_template` names corresponding to pinned permitted definition
IDs. Missing/null templates fail with a missing-default error: this port does
not configure an implicit default or silently choose the first permitted entry.
Templates map to PSA definitions, not Pi-local template files. Define the intended
model, tools, skills, extra prompt, and hooks on each pinned definition. Ordinary
PSA model/session configuration rules apply; no credentials are copied to probes.

## Goal mode

The interactive Goal bundle installs sequential `set_goal` and `set_goal_status`
tools plus a goal-aware stopping hook. State includes goal, status, optional
reason, and generation. Setting/replacing a goal starts it. Only blocked goals
can be resumed; blocked/completed transitions require a nonblank reason.
Completion clears the goal. Evidence quality and explicit user authorization
are model-facing rules, not claims of mechanically verified evidence.

Goal state is saved as versioned, namespaced diagnostic events, excluded from
model history. It is restored from the current agent lineage rather than only
from runtime memory. Agent forks receive independent owners and hydrate their
inherited state. Worker retirement does not reset goals.

For a normal no-tool final response, a running goal appends an authoritative
current-goal/generation reminder and continues. Reminders remain append-only;
old reminders are history, not current goal state. Normal queued user messages
wait until the active goal stops; steering is delivered at the next worker
boundary. Explicit interruption still interrupts the agent. When no goal is
running, the hook settles normally. Other completed no-tool stop reasons settle
without generating another goal reminder; failed/incomplete inference retains
the runtime's interruption behavior.

`StopWithoutTools` defers to this policy when an interactive Goal bundle is
present, so it can remain in explicit hook configuration without overriding the
goal hook. There are no Pi footer widgets, slash-command control endpoints, or
request-history filtering in this port.

## Subagent tools

- `create_agent(name, task, agent_template?)`: starts a fresh, noninteractive child
  in the same session using an explicitly permitted definition. It is NOT a
  session fork. Names are session-unique, ASCII letters/digits/underscore/hyphen,
  with length 1 through 80. Fork ancestry (`Agent.parent_id`) is not repurposed
  for coordination parentage.
- `send_message(agent_name, message)`: sends attributed steering within the
  caller's group. Idle agents resume in their existing conversations. Self-send
  and blank messages are rejected.
- `wait_message(timeout?)`: defaults to 120 seconds, with a positive maximum of
  600. Waits for acknowledged group delivery, steering availability, timeout, or
  invocation cancellation. An already-ready group acknowledgement contains
  message IDs/senders; otherwise available steering immediately returns
  `steering_available` without pretending its contents were consumed. Bodies
  arrive through steering, once. Already-consumed steering and late
  acknowledgements do not wake a future wait.
- `list_agents(all?)`: lists direct children by default, or all members of the
  caller's group with `all: true`, including idle members and parent relationships.

The shared actor owns membership, name reservations, parent/group relations,
message acknowledgements, waiters, and active run tracking. Separate roots in a
session have separate communication groups. Default policy limits active child
workers to eight and creation depth to four. Waiting workers consume capacity;
full capacity is rejected rather than queued. These initial limits are fixed,
not a new server/template configuration surface.

Logical run lifecycle notifications are independent of worker task generations.
Steering an already-running agent belongs to its current run; new work on an
idle agent starts a fresh run ID. Completion/failure/interruption updates group
status and reports to the parent, with stale/duplicate completion suppression.
An ordinary completed parent does not destroy independently running children;
interrupted/failed ownership scopes interrupt active descendants. Automatic child
reports do not resume an interrupted/failed parent, preventing interruption
feedback loops; an explicit message can still start new work. Waiting refuses
when nobody else in the group can make progress, while allowing sibling parallel
create/send operations to establish progress.

Coordinator metadata is checkpointed through existing trace events and restored
using session-scoped read APIs. Fork-inherited checkpoint identities are not
reused as new-session identities. Checkpoints and agent creation are not one
transaction: a crash before a checkpoint can leave a durable, reserved agent
record without recovered coordination membership. Pending/in-flight delivery is
runtime state, not a durable exactly-once messaging queue.

## Execution and lifecycle

Adjacent parallel tool calls run concurrently. Sequential tools form barriers
before and after their invocation; Goal setters use those barriers. Results are
persisted by call ID in completion order, not necessarily declaration order.
Invocations share an immutable base projection within each parallel batch and
have stable snapshots; their auxiliary saves are serialized through
the worker-owned actor mailbox and update that invocation's projection. Other
parallel invocations do not automatically refresh their snapshots.

One worker owns control-message consumption and authoritative event persistence.
Interruptions cancel all active tool futures, respecting individual cleanup grace
periods. Dropping an invocation does not retract an already enqueued event save.
Session updates affect later invocation batches, not in-flight snapshots.

`Sessions::wait_for_steering` is a cancellation-safe, level-triggered notification:
it observes queued steering without consuming it. Registration/checking are
serialized by the manager, so steering cannot be lost between them. Runtime
shutdown wakes waiters and cleans session resource owners as well as per-agent
bundle owners. Coordinator-owned asynchronous jobs remain interruptible during
shutdown and do not block its mailbox on session-manager requests.
