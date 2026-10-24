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
IDs. Missing/null templates fail: no implicit default is configured.
Templates map to PSA definitions, not Pi-local template files. Define the intended
model, tools, skills, extra prompt, and hooks on each pinned definition. Ordinary
PSA model/session configuration rules apply; no credentials are copied to probes.
The native `create_agent` tool accepts an optional typed `model` selector, not a
provider/model string: `"Global"`, `"Parent"`, or, for example,
`{"Fixed":{"model":"openrouter/vendor/model","effort":{"Fixed":"High"}}}`.
It must exactly match an allowed selector, including its effort policy. Fixed
selectors may use effort `"Parent"` or `"Global"` only when configured that way.
The allowed-template identity prompt carries the full selectors. Omitted/null
selects the sole policy, or the first when the template's `default` is true.
Registration normalizes single-model configurations to `default = true`;
multiple choices with `default = false` require explicit selection.

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

## Subagent tools and core ancestry

Agent ancestry is persisted as `Agent.subagent_parent_id` and exposed through
agent create/get/list responses and snapshot events, including without the bundle.
The parent must be in the same session. `Agent.parent_id` remains fork lineage.
Session forks remap ancestry to copied agent IDs. Existing checkpoint logs are
not imported; preexisting agents default to no subagent parent.

- `create_agent(name, task, agent_template?)` starts a fresh noninteractive child
  using an explicitly permitted definition. Missing/null templates are rejected.
  Creation establishes parentage atomically and rejects duplicate session names.
- `send_message(agent_name, message)` sends typed, attributed steering to any
  other agent in the session. Blank messages, self-send, and ambiguous names fail.
- `wait_message(timeout?)` waits for core steering availability without consuming
  it, returning `steering_available` or `timeout`. The default is 120 seconds,
  with a positive maximum of 600. There are no message acknowledgements.
- `list_agents(all?)` lists direct children, or every session agent with `all=true`.

The session is the canonical interaction boundary; there are no agent groups,
fixed child concurrency/depth limits, or automatic interrupt cascades. A small
bundle actor tracks active runs, wait registrations, and pending create/send
operations. It rejects waits when no session run can progress outside a wait,
after briefly deferring and allowing pending operations to establish progress.
It observes runs without the bundle too. Timeout/cancellation releases waits.

`AgentRequest::SubagentMessage` uses the ordinary steering queue. Its optional
source contains agent ID/name; supplied IDs are checked against the session and
names are resolved by the server. Transcript messages use `MessageType::Subagent`
and preserve source metadata. Model projection renders them as attributed
user-role context. The Rust client exposes `send_subagent_message`.

A session lifecycle callback installed by the bundle sends completion/error/
interruption reports to a child's parent when the parent has the bundle, even
if the child does not. Logical run IDs prevent duplicate reports across worker
generations. Background reports cannot restart interrupted/failed parents;
explicit messages retain ordinary steering behavior. Reports are middle-truncated
to 30,000 bytes. Queued requests and report delivery are best effort across crashes.

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
bundle owners. Bundle-owned asynchronous jobs remain interruptible during
shutdown and do not block its mailbox on session-manager requests.
