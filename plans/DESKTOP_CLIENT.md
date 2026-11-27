# Desktop client: initial UI and API plan

Status: initial desktop client and API implementation added. This document records
its design and agreed scope; see `client/psa_desktop/README.md` for running it and
`lib/psa/API_OVERVIEW.md` for the concrete endpoint contracts.

The desktop WebView renderer is the default `native` feature; building with
`--no-default-features` keeps workspace tests independent of native GUI
development libraries. Web/mobile/TUI
shells remain future work. Duplicate-agent views are supported by workspace state
but deliberately have no creation action in the initial GUI.

## Crates and ownership

Create only these client crates initially:

```text
client/
  client_shared/   # UI-independent state, commands, projections, connection logic
  psa_gui/         # Shared Dioxus components, styling, navigation, workspace layout
  psa_desktop/     # Executable, desktop platform implementation, local persistence
```

`psa` remains the thin HTTP/SSE client and wire types. Future `psa_web` and
`psa_mobile` implement the same GUI platform interface. A future `psa_tui` uses
`client_shared`, not `psa_gui`.

`psa_gui` receives a configuration struct and one object-safe platform interface.
Desktop owns async loading/saving of settings and platform integration. Keep
platform methods driven by real features; no speculative native integration API.
No server/probe embedding or automatic startup in the initial desktop client.

## Application shell

Dark theme initially, with consistent color, spacing, typography, and focus tokens.

- Left activity rail remains visible at roughly one icon wide.
- Expand/collapse control at the top; settings at the bottom left.
- Sidebar has three explicit states:
  - collapsed: activity rail only;
  - expanded session browser;
  - expanded current-session details and agent tree.
- Switching between browser/details is explicit. Collapsing remembers the last
  expanded mode. Current-session details follow the active editor tab.
- Remaining space is a tabbed workspace, not one globally selected transcript.
- Changing the probe filter does not close tabs belonging to other probes.

## Session browser

A probe dropdown at the top filters the browser. Show probe labels and connection
state; use IDs when labels are missing or ambiguous.

Group sessions by the exact working-directory path within the selected probe.
These are paths on the probe, not local desktop paths; do not canonicalize them
against the desktop filesystem. Initially use collapsible folder groups, not a
nested filesystem explorer.

Ordering:

1. Sort sessions by `Session.updated_at` descending.
2. Group by working directory, preserving that order inside each group.
3. Sort groups by their newest session's `updated_at` descending.
4. Use stable ID/path tie-breakers so equal timestamps do not cause jitter.

This is equivalent to repeatedly taking the newest remaining session and showing
its entire folder group. Include forked sessions with a fork marker. Use existing
`updated_at` semantics, including metadata changes and agent creation; do not add
`last_event_at` or fetch transcript history to compute recency.

Each row shows session name and a busy indicator from `Session.busy`. Folder rows
may show aggregate activity if any child is busy. Busy does not change the sort
key. Disconnected state must not be presented as confidently idle.

## Current-session sidebar

Show the active tab's session name, working directory, probe, session default
model/effort, activity, and agent tree.

Build ancestry from `Agent.subagent_parent_id`, **not** `Agent.parent_id` (fork
lineage). Support multiple roots. Selecting an agent focuses its existing tab
anywhere in the workspace, or opens it using preview-tab behavior if absent.
Agent rows show execution state. Hidden agents are
hidden by default with an explicit way to reveal them; preserve understandable
ancestry when a hidden ancestor has visible descendants.

Session selection chooses the last-viewed visible agent, then the first visible
interactive root, then the first visible root. If none exists, show an empty
session view rather than selecting a hidden agent silently.

## Tabs, previews, and splits

Represent the workspace as a split tree. Leaves are tab groups, each with an
active tab and at most one preview tab. Tabs reference `(session_id, agent_id)`;
keep view IDs separate so the same transcript can appear in multiple panes.

- First search all tab groups for the selected session/agent; if already open,
  focus its tab and pane without replacing another preview.
- Otherwise single click opens/reuses an italic-title preview in the focused
  tab group. Another new selection replaces that group's preview, not a
  permanent tab.
- Double click on a browser item or preview title makes its tab permanent.
- Initially there is no UI action to open duplicate views of an agent. Keep
  independent view IDs and shared transcript state internally so a future
  duplicate/split-view action does not require redesign. If restored/future
  state contains duplicates, focus the most recently focused matching view.
- Editing a draft or submitting a message promotes a preview to permanent so
  navigation cannot silently discard work.
- Drag tabs to reorder or move between groups; edge drop creates a split.
- Horizontal/vertical splits are resizable. Remove empty groups cleanly.
- Tabs from different sessions and probes may coexist.

Share transcript/network state across views, but keep scroll position, expansion
state, and focus per view. Draft ownership is per session/agent rather than per
view, preserving a coherent composer if duplicate views are added later.

Restore tabs, split layout, focused tab, sidebar mode, selected probe, and
last-viewed agents across restarts, scoped to server identity. Save stable IDs
and layout, not live connections or transcript caches. Missing sessions/agents
must not prevent startup; transient connection failure must not erase the saved
workspace. Keep unsent drafts in memory initially, not persisted.

Explicit split controls can land before drag/drop, but drag/drop and restart
restoration are part of the planned desktop UI, not API requirements.

## Transcript and composer

Within each pane, center the transcript with a comfortable maximum width
(initial target around 80–90 characters) and responsive side gutters. The composer
uses the same column width and stays at the bottom of that pane. Narrow splits
shrink the column rather than forcing a desktop-wide minimum.

Initial event rendering:

- Human/agent/harness messages with readable role attribution.
- Logs, with distinct error styling.
- One generic tool-call component for every tool: function name and compact
  argument preview, expandable arguments and associated results.
- Match tool calls/results by call ID within their agent transcript, not by name
  or adjacency. Preserve running/error states and tolerate incomplete history.
- Compaction as an expandable item, collapsed by default.
- Safe fallbacks for unsupported content blocks and provider-native events;
  never assume every payload is displayable text.
- Keep bookkeeping events out of the main conversation unless useful as a small
  status item or in a diagnostics view.

Apply provisional updates in place and replace them on commit without duplicate
rows. Auto-follow only while near the bottom; otherwise offer a new-activity
indicator. Bound rendering work and avoid mounting an entire large history.

The composer targets its pane's agent and defaults to `MessageDelivery::Steer`.
Provide an interrupt control. Display asynchronous request failures using the
returned request ID; accepted does not mean delivered or completed. Do not
silently retry ambiguous message submissions after connection failure.

Outstanding client questions also need a usable answer UI, including query
removal when answered elsewhere or cancelled.

## Slash commands and models

Implement a client-side command registry and autocomplete, not prompt text sent
to the model. Keyboard support: up/down selection, Enter acceptance, Escape
dismissal. Preserve a way to send literal leading slash text.

`/model` changes the **session default** model/effort of the focused tab's session,
using the existing session-update API. Label that scope explicitly. Individual
agent model changes are not supported and will not get a new endpoint.

Show favorited models by default and an explicit “all models” option. An empty
favorites list should offer settings/all-models rather than appear broken. Always
show the current session default even when it is not favorited. Favorites are
per server, using its existing model pins (see settings).

Do not imply pinned agents automatically follow a session-default change. The
sidebar shows session defaults, not a fabricated effective model for each agent.

## Desktop settings

Store desktop settings in `<psa-data-dir>/desktop.toml`, using the same PSA data
directory convention as server/probe. On Linux this defaults to
`~/.local/share/psa/desktop.toml`; allow a desktop data-directory override.

Initial settings: API URL, API client key, model favorites management, and basic
UI preferences. Store connection settings, UI preferences, and restored workspace
state in `desktop.toml`. Favorites use the selected server's existing persisted
model pins; the settings screen edits those through `psa`, not an independent
local favorites list. Keep desktop settings separate from `config.toml`,
`probe.toml`, and `auth.toml`; never copy provider credentials into desktop config.

Write settings atomically with async I/O. Since the requested file includes the
client key, treat it as a secret-bearing file: owner-only permissions where
supported, masked input, no key in logs/error text. This is plaintext persistence,
not keychain storage. Changing API connection settings cancels old subscriptions
and isolates cached state by server identity.

Favorites are scoped to the connected server and shared with its other clients.
Provider plus API model name defines identity. `desktop.toml` is not their source
of truth; changing servers loads that server's pins. This explicitly interprets
“per-server favorites” as the existing server-wide pins rather than local lists.

## API audit: current support and gaps

Verified against the current local source, not assumed from endpoint names.

### Supported

- `Session` includes probe, working directory, default model, and live `busy`.
- `SessionUpdated` carries busy/metadata changes on session/agent SSE streams.
- Agents include subagent parent relations, execution state, and hidden metadata.
- Events have timestamps; tool calls/results have correlation IDs; compaction,
  logs, messages, pending events, and client questions have typed payloads.
- Steer delivery, interrupt requests, session-default model updates, model
  catalogs, and server-wide model pins already exist.

### Agreed API work

1. **Extend the session query API.** Add a flat listing mode including forks,
   optional probe filter, `updated_at` descending ordering, and pagination with
   stable ID tie-breakers. Retain explicit roots/direct-children queries. Return
   session metadata including busy state; no last-event field or transcript
   queries are needed. Page merging and live updates must deduplicate by session
   ID. Directory groups are formed from loaded summaries; do not imply a partially
   loaded group is complete. Consider directory-group pagination if large lists
   make the flat approach awkward, but do not require it for the initial client.
2. **Add a separate global filtered event API.** This is distinct from transcript
   event endpoints and can subscribe across all sessions, not one connection per
   session. Initially only support `SessionUpdated` as a data event; structure the
   selection as a typed allowlist so additional event types can be added later.
   Do not expose raw transcript events or build a generic event bus now. An
   optional probe filter can reduce traffic without removing global scope.
   Publish a full session upsert on creation, metadata/default-model changes,
   busy transitions, and event commits/agent creation that change `updated_at`.
   Existing busy/metadata publication alone is insufficient for recency updates.
   Use the normal publication ordering and publish only committed state.
   Changes may be coalesced per commit, not per token/provisional update.
3. **Define global-feed bootstrap and recovery.** An initial ready boundary and
   bounded buffering allow subscribing before fetching session pages and then
   reconciling updates without missing changes or overwriting newer state.
   Use an ordering token/revision, not `updated_at` alone: busy changes do not
   advance that timestamp. On reconnect or overflow, signal resync and reload
   summaries. Durable replay is not required initially. Snapshot/ready/resync
   controls are protocol metadata, not additional selected data-event types.
   Session deletion is not currently a supported operation; add removal
   notifications if/when deletion is introduced. Probe/model changes can be
   refreshed separately rather than broadening this feed prematurely.
4. **Extend event endpoints for recent/backward history.** Support a bounded
   latest committed page and exclusive `before` pagination for older history,
   both at session and agent scope. Keep displayed events chronological. Return
   explicit older-page and live/head cursors; session positions and agent event
   IDs retain their distinct scopes. Older pages must not replace current pending
   events/questions. Bootstrap the live stream from the captured head so new
   events between history fetch and stream connection are replayed without gaps.
   Reconnect/resync and provisional-to-committed promotion must remain correct.
   Preserve existing forward pagination; reject ambiguous parameter combinations.

### Deliberately not required

- No `last_event_at`; use existing `Session.updated_at` everywhere for recency.
- No individual-agent model mutation API or new effective-model field. Session
  PATCH is the only model-change operation used by the GUI. Existing configured
  agents can be pinned; session-default changes do not override them.
- No new favorites/preferences API; reuse server-wide model pins.

### Remaining implementation considerations

Agent metadata bootstrap is separate from event subscription: stream snapshots
contain pending events/questions, not a full agent-list snapshot. Establish
ordered bootstrap/reconciliation instead of trusting fetch-then-subscribe. Share
and reference-count subscriptions for open views of a session; the global summary
feed updates sidebar metadata without loading unopened transcripts. Test initial
fetch races, paging during updates, reconnects, and out-of-order HTTP responses.

Relevant sources: `lib/psa/src/api/{types,events,models,client,routes}.rs`,
`runtime/psa_server/src/db/{events,sessions,agents}.rs`,
`runtime/psa_server/src/agents/{publication,sessions/configurations}.rs`, and
`runtime/psa_server/src/components/hooks/session_model_update.rs`.

## Suggested delivery order

1. Implement and test the agreed API additions: flat/filterable ordered session
   queries, global filtered session-update feed with safe bootstrap/resync, and
   recent/backward transcript pagination. Keep model changes on session PATCH.
2. Scaffold crates, platform/config injection, dark shell, and desktop settings.
3. Implement/test summary projection, grouping, agent tree, and connections in
   `client_shared`; render the two expanded sidebar modes.
4. Build preview/permanent tabs, workspace-wide focus-existing behavior, split-tree
   state, and restart restoration with reducer tests; add split controls, then
   drag/drop and resize behavior. Keep duplicate-view support internal only.
5. Add live transcript, generic tools, logs/compaction, question UI, composer,
   slash-command popup, and session-default `/model`.
6. Test multiple sessions/probes, duplicate views, reconnect/resync, large history,
   stale queries, dirty previews, settings failures, and empty/offline states.

## Confirmed decisions

- `/model` changes the focused tab's session default only; no agent model changes.
- Favorites are per server, interpreted here as existing shared server pins.
- Forks appear in directory groups with a marker.
- Session selection uses last-viewed visible agent, then visible interactive root,
  then visible root.
- Sidebar selection focuses an agent's existing tab across all panes. Duplicate
  views are supported internally but have no initial UI action.
- Restore tabs and split workspace after restart.
- Sort with existing `updated_at`; no last-event timestamp addition.
- Add the separate global filtered event API, initially only `SessionUpdated`,
  and extend session queries and recent/backward event pagination as needed.

## Remaining product questions (nonblocking defaults)

1. When activity changes ordering, should rows move immediately under the
   pointer? Default: defer reordering during active pointer/drag interaction.
2. Persist unsent drafts across restarts as well as tabs? Default: no, drafts
   remain in memory initially. Promote an edited preview so ordinary navigation
   does not discard its draft.
3. Message shortcuts? Default: Enter sends, Shift+Enter inserts a newline;
   autocomplete consumes Enter while a completion is selected.

API field names, feed ready/resync protocol, cursor encoding, and page sizes are
implementation details to settle with tests, not additional product blockers.
The requested API key storage remains plaintext `desktop.toml`, with restrictive
permissions and redaction.

## Initial implementation verification

- `cargo fmt --all -- --check` and `git diff --check` pass.
- `SQLX_OFFLINE=true CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 cargo test --locked --workspace`:
  681 tests passed, one ignored (including 21 shared-client, seven GUI, and five
  desktop settings tests).
- Strict Clippy passes for all targets of the three new client crates.
- `cargo check --locked -p psa_desktop --features native` passes in a Nix shell
  providing GTK/WebKitGTK dependencies. The interactive native window has not
  been manually smoke-tested.
- Workspace-wide strict Clippy remains blocked by existing server/API warnings
  outside this implementation's changed code. No unrelated lint cleanup was made.
- An initial fully parallel run exposed the existing scheduling-sensitive
  `parallel_wait_then_create_receives_child_completion_and_keeps_agents_addressable`
  test. It passed 21 targeted retries and the final workspace run. The underlying
  queued-to-started coordination race was not broadened into this client task.

Current UI limits: no duplicate-view creation action (by design), binary attachment
placeholders rather than media viewers, and local DOM scroll/expansion state may
reset when moving a tab between panes. Web/mobile/TUI shells remain unimplemented.
