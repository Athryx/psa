# Bash tools and retained bundle resources

Enable the group on an agent definition:

```json
{"bundles": ["FileTools", "Bash"]}
```

`Bash` installs the model-facing `bash`, `interact_bash`, and `close_bash` tools
(`ToolId::Bash`, `InteractBash`, and `CloseBash`). These three tools cannot be
installed independently: they share the bundle's actor-owned session registry.
Configure the usual `DefaultStartup` and `ToolDefinitions` hooks to initialize
context and expose the tools; bundles do not implicitly install those hooks.

## Lifetime and ownership

The session-manager actor retains installed components by agent ID. Worker
retirement or failure does not destroy a Bash session; a later worker generation
borrows the same instances. Each agent, including a forked agent, has its own
registry. Resource ownership lasts for the current runtime, not across server
restarts or remote process reconnection. No static/global session registry is
used.

Runtime shutdown stops workers, then concurrently cleans up retained agents.
Within each agent, bundles clean up in reverse installation order. Bash cleanup
closes its sessions and waits for its actors, subject to bounded cleanup; Drop
aborts remaining owned tasks if graceful cleanup is interrupted. Probe
connections stay available until runtime-resource cleanup finishes. Cleanup
failures are logged and reflected by runtime shutdown errors.

## Model-facing interface

The implementation follows the installed extension at
`pi_extensions/pi/extensions/bash`, including its descriptions, field
names/descriptions, wait defaults, status text, interval output and truncation.
Pi-specific UI/programmatic details are omitted; ordinary tool results use PSA's
existing text content blocks.

- `bash`: required `command`; optional `yield_time_ms` and `session_name`.
  Wait defaults to 60,000 ms. Each call starts a fresh shell in the invocation's
  session working directory. If it is still running, return its background name,
  log path, and instructions for interaction/closing.
- `interact_bash`: required `session_name`; optional `stdin` and `yield_time_ms`.
  Stdin is sent verbatim, without adding a newline. Empty/omitted stdin means
  polling. Wait defaults to 1,000 ms when sending nonempty stdin and 10,000 ms
  when polling. Return only output since the previous successful collection.
- `close_bash`: required `session_name`. Kill the session's process group, wait
  for completion, collect unread output, remove the registry entry, and retain
  the log. Ongoing invocation cancellation does not interrupt close after the
  shared typed-tool boundary's initial cancellation check.

Waits must be integers from 0 through 300,000 milliseconds. The extension's
legacy `yield_timeout_ms` is accepted, with `yield_time_ms` taking precedence.
Optional fields may be omitted but not explicitly null. Explicit session names
retain their spelling; whitespace-only names are invalid. Automatic names are
`bash1`, `bash2`, and so on, skipping names already registered. Names are reserved
before asynchronous startup. Completed sessions are removed when their result is
observed. One active collector/close per named session is allowed.

Nonzero exit codes set the tool error flag and append the exit-code message.
Signal-only termination with no numeric exit code follows the extension's
non-error behavior. Durations are rendered with one decimal second. Foreground
successful untruncated output uses `(no output)` when empty; other empty results
use `(no new output)`.

## Output and logs

Although the copied tool descriptions say “last” lines/bytes, the extension's
actual implementation keeps BOTH ends. Displayed intervals are bounded to
2,000 lines and 50 KiB, counting the middle-omission marker. Line-based truncation
keeps up to 999 head lines and 1,000 tail lines plus the marker. UTF-8 decoding is
streamed and snapshots carry the extension's line/byte truncation notices.

Every process continuously logs combined observed stdout/stderr to a private,
exclusively created remote `pi-bash-<24 hex digits>.log` under the probe's
`TMPDIR` (default `/tmp`). No server-local filesystem or server environment is
used. Output buffering is bounded; the full output is not kept in server memory.
Each remote append operation is completed before its preview is returned, so
transport enqueue alone is not mistaken for a remote write acknowledgement.

Successful, untruncated foreground commands with exit code zero delete their
logs. Background, failed, truncated and explicitly closed commands retain logs.
Use the `read` tool to retrieve retained logs. Cancelling initial `bash` kills
and removes the session and deletes its log; cancelling `interact_bash` only
stops that wait and preserves the session and unread output.

## Shell and environment mapping

Commands use the remote probe-service environment and its prepared-tool PATH,
not a user login-shell environment and not the server's environment. The default
shell selection tries remote `/bin/bash`, then `bash` on PATH, then `sh`.
There is no per-session shell-path or command-prefix setting in this port.

The tool clears inherited `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`,
`PI_MODEL`, and `PI_REASONING_LEVEL`, then sets current invocation values:

- `PI_SESSION_ID`: PSA's session ID as a decimal string.
- `PI_PROVIDER`: `openai-codex` or `openrouter`.
- `PI_MODEL`: the active model API name.
- `PI_REASONING_LEVEL`: the active effort level.

`PI_SESSION_FILE` remains unset because PSA has no Pi session-file counterpart.

## Intentional runtime differences

The existing Linux probe immediately kills remaining group members when the
shell leader exits. The source extension instead waits for stream EOF or 100 ms
of output inactivity after leader exit, resetting that idle timer on each chunk.
Therefore detached/background descendants behave differently. Neither behavior
promises to kill descendants that escape the process group.

The source uses immediate SIGKILL on Unix, not SIGTERM followed by escalation;
PSA uses the existing group-kill operation. Startup/log IO and shutdown have
additional bounded waits so failures cannot indefinitely block cleanup.
Disconnecting the probe cancels remote calls; sessions are not reattached after
reconnect. The port does not inject the Pi extension's shutdown-time custom
stopped-session message; cleanup diagnostics use the server/runtime error path.

Tests cover source-exact output formatting and previews, real-probe shell/stdin/
log/kill behavior, and resource retention across worker generations and runtime
shutdown. See `.agent/skills/making-tools/SKILL.md` for bundle authoring rules.
