---
name: making-tools
description: Add or update native PSA agent tools using typed Serde arguments, llm_api schemas, existing AgentContext capabilities, shared prompt formatting, and the typed execution macro.
---

# Making PSA tools

Use this skill whenever adding or changing a native agent tool in this repository.
Paths below are relative to the repository root.

## Capability boundary: stop instead of expanding the harness

Before implementing a tool, inspect the APIs below and determine whether they can
express its behavior. If they cannot, STOP and tell the user:

1. Which requested behavior is blocked.
2. Which existing capability is insufficient and why.
3. The smallest proposed API/protocol/runtime additions needed.

Do not add new probe operations, server endpoints, database behavior, agent/runtime
features, or AgentContext capabilities just to make the tool work. Do not bypass
these boundaries using direct database access, server-local filesystem/process
operations, private worker state, or a new transport. Wait for the user to approve
those additions as separate work. Tool registration, argument types, tool-specific
logic, prompt text, and tests are ordinary tool-authoring changes, not permission
to introduce new general-purpose capabilities.

## Files and required structure

- Each concrete tool goes in its OWN snake_case file under
  `runtime/psa_server/src/components/tools/`, e.g. `read_file.rs`. Keep its argument struct,
  concrete tool struct, inherent implementation, and `impl Tool` together.
- Agent components live under `runtime/psa_server/src/components/`: `bundles/`, `tools/`,
  and `hooks/` hold their respective implementations. Shared invocation
  cancellation lives in `components/cancellation.rs`.
- `tools/mod.rs` owns common infrastructure and dispatch, not concrete tool logic.
- Use `tools/noop.rs` and the Noop bundle as working references. `ToolId`
  identifies tools; every tool is installed by a bundle. There is no standalone
  tool configuration type, agent/defaults `tools` field, or tool config factory.
  Follow bundle installation and existing definition dispatch; do not create a
  second model-facing registry or execution path. CodeMode has an explicitly
  separate, internal function registry: participating bundles install functions
  with `installer.add_codemode_function(...)`, often using the existing tool
  adapter and shared bundle handles. It is not an agent configuration list.
- Adjacent tools run concurrently by default (`ExecutionMode::Parallel`). Override
  `execution_mode()` with `Sequential` when an operation must be a barrier before
  and after its invocation; Goal setters and CodeMode are working examples. Do not
  depend on declaration-order results or on another parallel invocation's side effects.
- Ordinary production concrete tools MUST use a typed argument struct and
  `typed_tool_execute!`. The schema-configured `record_result` tool is the
  deliberate dynamic-schema exception, using the server's `ResultSchema`
  validation rather than a fixed Rust argument schema. Do not handwrite JSON field extraction or a separate
  JSON-valued implementation of `Tool::execute`.

The pattern is:

```rust
use llm_api::api::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use super::{Tool, ToolOutput, ToolSystemPrompt, typed_tool_execute};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExampleArgs {
    /// Absolute path, or a path relative to the session working directory.
    path: String,
}

pub struct Example;

impl Example {
    // Implement only the real execution behavior in an inherent async method:
    // async fn execute_typed(
    //     &self,
    //     arguments: ExampleArgs,
    //     context: &mut AgentContext<'_>,
    //     cancellation: Cancellation,
    // ) -> ToolOutput
}

// Construct the unit struct directly: let tool = Example;
// Tool/Hook traits have no required `new` constructor. Stateful components use
// inherent constructors with explicit shared handles supplied by their bundle.
// In impl Tool for Example:
// - tool_definition directly returns ToolDefinition::for_type::<ExampleArgs>(
//       "example", "Tool-specific description kept in this file.", Some(false),
//   ). Use the argument type as the single source of the schema.
// - system_prompt directly returns ToolSystemPrompt { tools, rules }, with
//   tool-specific directions and rule strings kept in this file.
// - cancellation_timeout directly returns the bounded cancellation grace period.
// - generate the JSON-to-typed bridge with:
//     typed_tool_execute!(ExampleArgs, execute_typed);
```

`tool_definition`, `system_prompt` (including directions/rules), and
`cancellation_timeout` may be implemented directly in `impl Tool`. Do not add
inherent helper methods solely to forward these trait methods. Only the actual
typed execution method needs to be inherent so the macro can delegate to it.
Extract other helpers only when they clarify meaningful logic or remove duplication.

The macro implements ONLY the trait's `execute` method. It checks cancellation,
decodes arguments with `llm_api::api::parse_tool_arguments`, formats errors through
`prompts::tool_argument_error`, and delegates to the inherent typed method. Invalid
inputs must not execute logic or cause side effects. Serde reports the first
invalid field, including nested field/array paths; do not replace these errors
with a generic success or dump the input payload into the response.

## Every tool requires a bundle

Define a Rust bundle for every tool feature, even an unrelated single unit tool;
the Noop bundle is the minimal working example. Related tools and hooks share
an owner through their bundle. Tools cannot be configured individually. Keep
each concrete tool in its own file; typed invocation arguments and the execution
macro are unchanged.

- Put the bundle owner/assembly logic in `runtime/psa_server/src/components/bundles/<feature>.rs`.
  Register its `BundleConfig` in `psa_api` and dispatch construction through
  `BundleKind::from_config(&BundleConfig, Option<&SessionBundleHandle>)` in the
  server, returning `Result<Self, InstallError>`. The runtime supplies session
  resources, not serialized configuration arguments. `Subagents` requires those
  resources to construct its owner; do not add an unbound fallback.
  `components/bundles/file_tools.rs` is the basic working example; bundle lifecycle tests
  demonstrate state shared by two constructed tools and a hook.
- Agent definitions select ordered bundle configurations, e.g.
  `"bundles": [{"FileTools":{"read_only":false}}, "Bash"]`.
  `Noop` is a unit bundle installing Noop. `CodeMode` installs a sequential
  Python REPL tool using its own bundle-owned Monty runtime.
  `FileTools { read_only }` requires a payload: false or omission inside that
  payload installs Read, Edit, and Write;
  true installs only Read. Bare `"FileTools"` is invalid. TOML uses
  `{ FileTools = { read_only = true } }` in the bundle array.
  `Bash` installs Bash, InteractBash, and CloseBash. All tools require bundles;
  there is no explicit tool list on definitions, configs, or defaults.
  `Goal { interactive_only }` adds goal tools/hooks;
  `AskQuestion { interactive_only }` adds only the native `ask_question` tool, using
  existing `context.query_client`, with no hook. Both require payload objects:
  `{"Goal":{"interactive_only":true}}` and
  `{"AskQuestion":{"interactive_only":true}}`; bare `"Goal"` is invalid. TOML uses
  `{ Goal = { interactive_only = true } }` (or `AskQuestion`) in the bundle array.
  The flag defaults to true inside the payload: noninteractive agents get only
  StopWithoutTools from Goal and nothing from AskQuestion. False fully enables
  it regardless of interactivity, including Goal persistence and continuation.
  Generated defaults include Goal with interactive_only = true, not AskQuestion.
  This installation gate never restricts the
  low-level `query_client` API, which any agent can use, including offline probes.
  `Subagents` adds session-wide tools using session-shared resources. These
  component tool IDs also require their corresponding bundle.
  Unit configuration variants serialize as strings; variants with construction
  arguments carry typed payloads. Definitions, defaults, and TOML use
  `HookConfig`, `HookConfigs`, and `BundleConfig`, not identity IDs.
  Hooks dispatch through `HookKind::from_config(&HookConfig)` and
  `Hooks::from_configs(&HookConfigs)`. `ExtraSystemPrompt { prompt: String }`
  owns its prompt; there is no agent-level `extra_system_prompt` field.
  `TypedResult { schema: ToolSchema, max_retries }` installs sequential
  `record_result` and a require-result completion hook. Its schema is a raw,
  explicit object-root Draft 2020-12 schema with local references, validated
  offline by the server's `ResultSchema`. The whole argument object is the
  result, without a wrapper. Do not generate a separate fixed schema for it.
  Valid data atomically saves Result and Idle; acknowledgment is only
  `Result recorded.`. Invalid data yields an error, never a Result.
  TypedResult, Goal, and ordinary StopWithoutTools are incompatible completion
  modes, including inherited defaults; explicit hook overrides must remove the
  ordinary default completion hook when selecting a bundle completion mode.
  At most one policy may be configured; none is allowed but provides no automatic
  completion or Result. Interactive-only Goal supplies ordinary completion for
  noninteractive agents. Non-running Goal
  includes Blocked: retain the paused goal while settling this run with a text
  Result. Completion reports prefer Result artifacts, serializing JSON as text.
  Three retries after the initial invalid/missing completion are allowed by
  default; the fourth violation interrupts with a reason, not a fake result.
  Zero fails immediately. Reset only on a new non-Running to Running transition,
  never on midrun steering. See `docs/AGENT_RESULTS.md` for full semantics.
  Bundles install in definition order. Configured `agent_create` hooks run before
  bundle-owned creation hooks so startup initializes context before bundle prompts;
  other stages run bundle hooks before explicit hooks.
  Duplicate bundle IDs, tool IDs, or model-facing tool names fail installation.
  Hooks preserve their configured stage/order and are not silently deduplicated.
- Implement `Bundle::install(&mut self, agent, session, &mut Installer)` without
  filesystem/network IO. Construct shared handles once and pass clones into
  concrete tool/hook instances. Register them with
  `installer.add_tool(ToolKind::YourTool(instance))?` and
  `installer.add_hook(HookStage::SessionUpdate, HookKind::YourHook(instance))`.
  All four hook stages are supported. Do not merely expand a bundle into ID
  lists: that would construct its state independently for each component.
- Keep mutable long-lived shared state in an actor using the workspace `actors`
  crate when ownership/lifecycle warrants it. Bundle owners are not cloned;
  components clone only handles. Do not install duplicate components through
  multiple bundles; there is no definition-level explicit tool list.
- Implement async `Bundle::cleanup(&mut self) -> Result<(), CleanupError>` for
  graceful resource shutdown. It should be idempotent. Cleanup callbacks run in
  reverse installation order after the worker releases its tool/hook references,
  and bundle owners are dropped afterward. Each callback is bounded by
  `cleanup_timeout()` (three seconds by default). All owners are attempted even
  if one fails, panics, or times out; failures are reported rather than discarded.
  Cleanup must not depend on new session-manager requests during runtime shutdown;
  shut down the bundle's own resources/actors directly.
- If installation fails, partially installed components and owners (including
  the failing owner) follow the same cleanup path. Do not leak tasks/resources
  when registering a later component fails.
- Implement synchronous `Drop` cancellation as a fallback for owners with live
  resources. `Drop` cannot await. It must remain safe if async cleanup was never
  called, timed out, panicked, or was forcibly interrupted.
- The session-manager actor retains installed components and bundle owners by
  AgentId for the current runtime. Idle worker retirement or worker failure does
  NOT clean up that agent's bundle resources. Later worker generations borrow the
  same tool/hook instances and shared state. A fork has a different AgentId and
  gets independent owners. Runtime shutdown stops workers first, then cleans up
  retained agents concurrently, with each agent's bundle callbacks in reverse
  installation order. Probes remain connected during resource cleanup.
- Session-shared resource owners live separately in `components/bundles/session.rs`.
  The manager lazily creates them and passes a `SessionBundleHandle` to each
  agent's independent component installation. Share actor handles, NOT an entire
  `InstalledComponents` or mutable hook set across agents. The Subagents bundle is
  the working example. Typed logical run lifecycle callbacks feed its progress actor
  and completion hook; core agent metadata owns parentage, and ordinary typed
  steering owns message delivery. Do not duplicate hierarchy/status in bundles;
  worker retirement alone does not mean an agent completed. Shutdown closes these
  services after signalling all worker interrupts so waiting invocations can exit.
- Loading persisted state for recovery does not install bundles. Runtime resources
  do not survive a server restart; process reconnection/resumption is not promised.
  Use invocation context for current session/model settings, not only the initial
  snapshots passed to install. Ownership scopes or runtime APIs beyond the
  existing agent/session ownership and coordination APIs still require a proposal
  and user approval.

## Schemas and prompts

- Derive `Deserialize` and `schemars::JsonSchema` on arguments and nested
  types. Use the direct `schemars` dependency in `psa_server`; no crate-path
  override attribute or `llm_api` reexport is needed.
- Prefer closed argument objects (`#[serde(deny_unknown_fields)]`). Describe fields
  clearly with doc comments; descriptions become schema annotations. Keep schema
  and Serde names/defaults/optionality consistent. Custom deserializers require
  explicit matching schema annotations and tests. Schema-only annotations do not
  enforce runtime domain constraints; validate those in the typed method before
  side effects, using existing capabilities.
- `ToolDefinition::for_type::<Args>(name, description, strict)` takes an explicit
  `Option<bool>` policy. Prefer `Some(false)` for non-strict mode; `None` uses
  provider defaults. Missing fields and explicit null are distinct. Do not convert
  optional/defaulted fields into required nullable fields just to satisfy a
  provider. Pass `Some(true)` only for a schema that passes the
  shared strict checks, including closed nested objects and all properties
  required. Verify strict support rather than stripping constraints.
- Static `tool_definition(ToolId)` returns `Option<ToolDefinition>`; it returns
  None for `RecordResult`, whose installed instance carries the exact configured
  schema. Expose instance definitions rather than inventing a static fallback.
- Use `ToolSchema::from(serde_json::Value)` only as a deliberate raw-schema escape
  hatch, not the normal tool-authoring pattern. Provider serialization and schema
  validation belong to `llm_api`, not individual tools.
- Keep tool-specific descriptions, directions, rules, errors, and response-formatting
  strings directly in the tool's own file. They do NOT have to go in
  `runtime/psa_server/src/prompts/`. Inline strings in trait methods are fine; use local
  helpers only when they clarify formatting or avoid meaningful duplication.
  Field doc comments are the schema's local field documentation. Reserve the
  shared `prompts` module for genuinely shared harness text and utilities such as
  `text`, argument-error formatting, and `truncate_middle`.
- `ToolSystemPrompt.tools` is optional tool-specific usage guidance;
  `ToolSystemPrompt.rules` contains shared rules. The definitions hook renders
  these as tools/rules system messages and deduplicates rules. Explain argument
  names, units, path resolution, side effects, and truncation behavior consistently
  with the generated schema. Do not hardcode a second schema in prompt strings.

## API available to tools

Inspect the actual definitions before use; these are capabilities, not permission
to reach around their ownership or lifecycle rules.

### `AgentContext` (`runtime/psa_server/src/agents/agent_context.rs`)

- `agent()`, `session()`, `definition()`: the invocation's snapshots. Session
  metadata includes the working directory, model, and probe identity. Definitions
  are pinned versions, not mutable global configuration.
- `tools()`: available tool instances; `projection()`: current agent projection.
- `probe() -> Result<&ProbeHandle, ProbeError>`: the invocation's remote connection;
  it can be unavailable. Do not silently fall back to the server's filesystem.
- `sessions() -> &Sessions`: async session/agent facade described below.
- `query_client(query, &mut cancellation).await`: sends a typed `psa_api::ClientQuery`
  scoped to the invocation's session/agent and waits for `ClientQueryResponse`.
  The runtime owns pending requests; clients receive them through existing SSE
  streams and respond through the query-response endpoint. No connected client,
  probe, or `interactive` flag is required. Requests wait until answered,
  cancelled, or runtime shutdown; wrap/drop the future for a caller-defined
  timeout. Dropping the future removes its pending request, including during
  registration. Errors use `agents::client_queries::ClientQueryError`; map them
  intentionally to tool/hook results. Query plumbing does not write transcript
  events or automatically change agent status. See `docs/CLIENT_QUERIES.md`.
- Parallel invocations have stable agent/session/definition/projection snapshots.
  `save_events` uses an actor mailbox to the worker's single persistence owner and
  refreshes the saving invocation's agent/projection; it does not refresh peers.
  Hooks retain their direct worker context. A dropped save waiter cannot retract
  an already enqueued commit; do not bypass this path with direct database writes.
- `save_events(&[NewEvent]).await`: persists and publishes auxiliary events and
  updates the projection. `save_event_batch(&[EventBatchItem]).await` uses the
  same worker-owned path for public events and server-private attachments; a
  private item must follow its public anchor in the same atomic batch. Only the
  public events are published and returned. The worker separately persists
  returned `ToolOutput` as the tool result and atomically attaches its private
  events; do NOT duplicate that response as an auxiliary event.
  Durable `EventType::Result` completion artifacts are separate metadata excluded
  from model history, not ordinary tool responses. Never synthesize one merely
  because a run failed/cancelled, or duplicate a completion within a run.
  Cancellation before commit produces no result, but an accepted database save
  cannot be revoked and may complete after cancellation. Recovery after a typed
  artifact commit but before its acknowledgment restores only the producing
  call acknowledgment, with no duplicate Result or Interrupted status.
- Async trace queries: `load_events(range)`, `load_all_events()`,
  `load_context_window()`, `load_events_since_last_inference()`,
  `load_last_inference_result()`, and `load_agent_envelopes(range)`. These are
  scoped to the agent, including inherited fork history. Use them rather than SQL.

### `Sessions` (`runtime/psa_server/src/agents/sessions/mod.rs`)

Existing async operations cover agent-definition create/get/list/update;
session create/get/list/update/fork; agent create/get/list (including immutable
`subagent_parent_id`); attributed `send_subagent_message`; busy queries; and
submit/steer/resume/interrupt requests. `wait_for_steering(session, agent,
&mut cancellation)` is a cancellation-safe, non-consuming, level-triggered wait
for queued steering; it returns `Cancelled` on cancellation and `Unavailable` on
shutdown. `await_result(session, agent, after, &mut cancellation)` waits for a
durable result using publication notifications; `after` excludes older artifacts.
Stopped/failed agents without a matching result return `ResultUnavailable`.
Agent creation accepts `hidden` visibility metadata; the server does not filter it.
`load_agent_events(session, agent, range)` validates session ownership
before scoped cross-agent trace reads. Use these facade methods for persistence,
publication, and runtime coordination rather than manipulating their internals.

### `ProbeHandle` (`runtime/psa_server/src/probes.rs`)

Existing remote operations are `read_file`, `write_file`, `list_dir`, `create_dir`,
`upload_dir`, `download_dir`, and `spawn`. `list_dir` returns collected directory
entries; file/archive/process operations return streaming calls. Paths must be
absolute UTF-8 paths on the PROBE. Resolve relative tool paths against
`context.session().working_directory`, never the server process working directory.

`psa_api::probes::Command` builds process requests (program, arguments, cwd,
environment, and stdio). Streaming `Call` exposes `sender.write(0, bytes)`,
`sender.close_stdin()`, `next_event()`, `signal()`, `kill()`, `wait()`, and
`into_output()`. Data events identify streams (stdout=1, stderr=2). Respect flow
control: consume output concurrently with large stdin writes, and drain both
output streams. `into_output()` collects all output and is unsuitable for
unbounded output. Dropping a live call requests remote cancellation.

### Cancellation and output

- All IO must be async. Keep mutable long-lived state owned by the appropriate
  actor/task; tools only use capabilities exposed to them.
- Use `Cancellation::is_cancelled()` and/or select on `cancelled()` during long
  operations. The macro's initial check does not replace cooperative cancellation
  within the actual logic. Do not detach uncancellable work. The grace timeout
  is for cancellation cleanup, not a normal operation deadline.
- Return `ToolOutput::success(content)` or `ToolOutput::error(content)`.
  `prompts::text(...)` creates text content blocks. Ordinary validation, missing
  paths, process failures, and probe errors should become useful model-facing
  errors without leaking credentials or the complete argument payload.
- `ToolOutput.error` does not change agent execution status. Use existing context
  events/facades for an intentional status/session change; do not mutate runtime
  state directly.

## Truncation policy

When no behavior is specified, prefer middle truncation using
`prompts::truncate_middle(text, max_bytes)` for bounded model-facing text.
It retains BOTH the beginning and end, inserts a visible middle-truncation
marker, respects UTF-8 boundaries, and counts the marker within the byte budget.
It returns `Cow<str>` (borrowed when unchanged). Tiny budgets use a shorter marker.

Explicit tool requirements take precedence over that default. When replicating
Pi tools, follow Pi's behavior, including `read`'s head truncation at 2,000 lines
or 50 KiB and its exact continuation notices. Do not substitute middle truncation
when compatibility requires another policy. Keep compatibility-specific
formatting in the tool's file; reuse the common middle helper when middle
truncation is appropriate rather than inventing another version.

Choose and document an output budget, and never split UTF-8 characters with
arbitrary byte slicing or `.truncate()`. Bound reads/stream buffers WHILE
collecting output: truncating after collecting unbounded data is not a memory
limit. For a middle-truncated streaming preview, retain bounded head and tail
buffers. For a Pi-compatible read, retain the bounded selected prefix while
counting the remaining input for continuation notices. Apply bounds to errors too.
The typed argument error formatter already uses the shared middle truncation.

## Tests and finishing

Test the generated schema AND execution behavior: valid typed delegation;
missing, extra, wrong-type, and nested/array arguments; no side effects on invalid
input; cancellation; probe failures; and byte-bounded Unicode output following
its specified truncation policy (retaining both ends for middle truncation).
Use the existing probe/context fixtures rather than new harness features. Add regression tests for provider serialization when changing
schema behavior. Run `cargo fmt --all` and relevant tests. Workspace builds need
prepared probe assets as documented in `runtime/psa_probe/README.md`.
