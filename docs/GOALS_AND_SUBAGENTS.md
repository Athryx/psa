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
  "bundles": [{"FileTools": {"read_only": false}}, "Bash", "Subagents"],
  "hooks": {
    "agent_create": ["DefaultStartup", "ToolDefinitions", {"ExtraSystemPrompt": {"prompt": "Complete the assigned task and report to your parent."}}],
    "after_request": ["StopWithoutTools"]
  }
}
```

```json
{
  "name": "coordinator",
  "bundles": [{"FileTools": {"read_only": false}}, "Bash", {"Goal": {"interactive_only": true}}, "Subagents"],
  "permitted_subagents": [42],
  "hooks": {
    "agent_create": ["DefaultStartup", "ToolDefinitions"],
    "after_request": []
  }
}
```

Create the coordinator agent with `interactive: true` for the Goal configuration
above. `Goal` requires a payload object; bare `"Goal"` is invalid. Omitted
`interactive_only` inside that object defaults to true. With true, noninteractive
agents install only `StopWithoutTools`, without Goal tools or continuation.
Set it to false to fully enable Goal
on noninteractive agents, including state persistence and continuation policy.
TOML uses `{ Goal = { interactive_only = true } }` in the bundle array.
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

When enabled by its `interactive_only` policy, the Goal bundle installs
sequential `set_goal` and `set_goal_status`
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
boundary. Explicit interruption still interrupts the agent. Active goals continue
even on token-limited output and do not record a completion Result. When the goal
is not running (`!GoalState::is_running()`), completion records a durable text
Result and settles Idle. This includes Blocked as well as completed/unset goals:
a blocked goal stays retained and paused, even though the current run settles.
Failure/cancellation does not synthesize results; a result save already accepted
by the database can still complete despite cancellation.

Goal, ordinary `StopWithoutTools`, and `TypedResult` are mutually incompatible
completion modes. Do not retain `StopWithoutTools` with Goal, including through
server defaults: an explicit `after_request: []` disables inherited completion
hooks while the bundle supplies its own. See [durable results](AGENT_RESULTS.md)
for JSON completion, retries, and fork-aware retrieval. There are no Pi footer
widgets or request-history filtering in this port.

### Goal commands

Users change goals with `AgentCommand::Goal(GoalCommand)`, typed in the GUI as
`/goal`. The command handler is installed with the goal tools, so the server
rejects goal commands when the tools are not installed for the agent. Changes
use the same transitions and persisted state as the tools; the user need not
give a reason. Each change also saves a `Harness` note telling the model what the
user did, in the same batch as the new state.

| Command | `/goal` arguments | Goal change | Agent status |
|---|---|---|---|
| `Show` | none | none; the result output describes the goal | unchanged |
| `Set { goal }` | any other text | set or replace, running | starts a run unless running |
| `Pause { reason? }` | `pause [reason]` | blocked; reason defaults to "Paused by the user" | unchanged |
| `Resume` | `resume` | blocked to running only | starts a run unless running |
| `Clear { reason? }` | `clear [reason]` | dropped without completion | unchanged |

Pause and clear never stop a run. Their note tells the model to stop working on
the goal; with the goal no longer running, its next final answer settles the run
with an ordinary text Result. Deliver goal commands with `CommandDelivery::Steer`:
a queued command waits until the agent stops running, which a running goal does
not do on its own. A rejected change, such as resuming a goal that is not
blocked, returns `Failed` with the reason and saves nothing.

## Subagent tools and core ancestry

Agent ancestry is persisted as `Agent.subagent_parent_id` and exposed through
agent create/get/list responses and snapshot events, including without the bundle.
The parent must be in the same session. `Agent.forked_from` remains fork lineage.
Session forks remap ancestry to copied agent IDs. Existing checkpoint logs are
not imported; preexisting agents default to no subagent parent.

The bundle emits its identity/instructions system prompt once on agent creation,
after configured startup hooks initialize context. It does not refresh identity
on requests, worker reloads, or server restarts. Forks preserve the inherited
prompt unchanged, keeping the existing model-history prefix cacheable.

- `create_agent(name, task, agent_template?)` starts a fresh noninteractive child
  using an explicitly permitted definition. Missing/null templates are rejected.
  Creation establishes parentage atomically and rejects duplicate session names.
  Cooperative cancellation lets creation and initial task submission finish within
  the tool's grace period, then requests child interruption and reports its ID.
  This is not rollback or an atomic startup guarantee; forced timeout can still
  cut the sequence short.
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

Subagent messages are `AgentRequest::Message` requests with a `source`; tools
send them with `Steer` delivery. The source contains agent ID/name; supplied IDs are checked against the session and
names are resolved by the server. Transcript messages use `MessageType::Subagent`
and preserve source metadata. Model projection renders them as attributed
user-role context. The Rust client sends them with `submit_request`.

A session lifecycle callback installed by the bundle sends completion/error/
interruption reports to a child's parent when the parent has the bundle, even
if the child does not. Logical run IDs prevent duplicate reports across worker
generations. Completion reports prefer the actual durable Result artifact over
an assistant preamble; JSON artifacts are serialized as text for the report.
Background reports cannot restart interrupted/failed parents;
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

`RuntimeHandle::wait_for_steering` is a cancellation-safe, level-triggered notification:
it observes queued steering without consuming it. Registration/checking are
serialized by the manager, so steering cannot be lost between them. Runtime
shutdown wakes waiters and cleans session resource owners as well as per-agent
bundle owners. Bundle-owned asynchronous jobs remain interruptible during
shutdown and do not block its mailbox on session-manager requests.
