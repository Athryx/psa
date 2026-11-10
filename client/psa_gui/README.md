# psa_gui

Renderer-independent Dioxus 0.7.10 UI. This crate deliberately does **not** enable
Dioxus's desktop renderer. `psa_desktop` owns the WebView, asynchronous settings
loading, atomic secret-bearing persistence and the Tokio runtime.

Mount `App` with a `GuiBootstrap` root context containing `GuiConfig { settings }`
and `Rc<dyn GuiPlatform>`. `GuiPlatform::save_settings` returns a boxed async
future without a `Send` requirement, permitting thread-local web/mobile platform
implementations. Saves are debounced through a bounded latest-snapshot writer and
contain stable settings/workspace IDs only, never transcript caches or drafts.
Platforms register `SettingsFlush` through `set_settings_flush` and await it before
closing; failed saves retain the latest snapshot for retry.

The UI uses `client_shared` workspace, session grouping, ancestry, transcript,
command parsing and reconnect policy. Network orchestration stays in `network.rs`:

- Subscribe to the global summary feed before fetching pages; revisions protect
  live upserts from late pages. Reconnect establishes a fresh ready boundary.
- Open views reference-count one live stream per session, including agent-tree
  updates. Session-position replay cursors are never confused with agent event IDs.
- Stream publications are frame-batched with bounded intake/backpressure. Routing
  happens before payload cloning; snapshots, removals and lifecycle barriers retain
  reducer ordering. Unchanged probe/catalog responses do not publish UI updates.
- Loaded history is separate from the bounded mounted row window. Passive browser
  observers recycle rows using stable IDs, measured spacers and pixel anchors.
  History merges do not replace live questions/provisional events.
- Server changes cancel old tasks and invalidate late HTTP responses. Mutating
  message submissions are never automatically retried; drafts survive failure.
- Provider-native/unsupported blocks have safe text fallbacks. No server content
  is inserted as raw HTML. Tool calls correlate results by call ID.

Tabs use previews, global focus-existing navigation, drag/reorder/move, four-edge
split drop zones, and draggable split dividers with arrow-key resizing. Inactive
views retain their controller/scroll state but freeze transcript subscriptions.
Closing the final view releases its loaded history; unsent draft text is retained
separately and reopening bootstraps fresh history. Workspace layout and sidebar
state restore through the injected settings.

## Rendering and input

`reactive.rs` provides copy-on-write collection snapshots and selective memo
subscriptions. Keep expensive projections out of broad `Signal<Ui>` reads; a
component subscribes to its small projection and reads coherent state with
`peek`. Request epochs and replay revisions are nonreactive bookkeeping.

`input.rs`/`input.js` own live text in the browser. Rust receives ordered edit
snapshots, never an ordinary controlled-value echo. Intentional replacements and
clears carry a mount/edit revision and are rejected after newer typing. Shared
draft identities survive duplicate panes and draft-to-agent promotion. Key and
pointer-submit actions carry fresh DOM snapshots; forms submit named DOM values.
Do not replace `apply_snapshot` with `DraftStore::insert`: the latter is an
intentional edit command. `interaction.rs` keeps slider/divider movement local
and publishes semantic commits, not every pointer event.

New-tab focus waits for the selected prompt to mount and become enabled; later
pointer/focus interaction cancels an outstanding asynchronous open request.
`wheel.js` routes wheel movement by the hovered scroll region without changing
keyboard focus. Short prompts route to their pane's transcript; long prompts and
nested tool output scroll locally first. Wheel units and physical detents are
normalized while precise pixel movement and zoom gestures remain native. A small
upward scroll disengages transcript follow immediately, without a render roundtrip.

`transcript_view.rs` renders shared, identity-comparable row payloads with explicit
preferences. Large collapsed details are materialized only when expanded. Add
rows through transcript reducers so display ranks and tool-result indexes remain
consistent.

Tool calls, system prompts, collapsed reasoning and similar events are compact
one-line items that expand in place. `tool_format.rs` supplies their summaries:
the target path, elided keyword arguments, and for `bash`/`codemode` a
highlighted preview of the leading source lines (recovered from partial JSON
while arguments stream). Consecutive items close up, and a run of the same tool
or kind shares one label with its rows indented beneath. Runs are laid out over
the mounted window only and the DOM stays a flat list of rows: a row marks a
closed-up gap with `data-row-gap`, which the viewport controller reads when
estimating recycled heights.

`UiPreferences::turn_dividers` marks each inference start, the boundary
between what a turn was sent (every row above) and what it produced (the rows
below). `Labeled`, the default, is a hairline row carrying the turn's number,
and, from the turn's end, its context size, output size and duration and a
button opening that turn's context tab; a running turn reads "running…".
`Line` is the hairline alone; `Hidden` draws nothing. A successful end draws
nothing; a failed one draws its report. Dividers sit between the rows of
separate model calls, so a run of like rows continues only within one turn.

Right-clicking a row opens a menu whose "Show context" opens the context tab of
the first turn the row was sent to (`Transcript::context_turn`), so the tab
always contains the row, whichever divider mode is set. Right-clicking selected text keeps the native menu, for Copy.

`context_view.rs` renders a `TabKind::Context` tab: a snapshot of what filled
one turn's context, as a segmented bar arranged by order, size or type, with a
totals table and the item list behind it. Clicking a block jumps to its first
item, expanding it when the block is that one item; list items expand in place
to the raw text the model was sent. The context's events are fetched once and
retained for this, and the view does not follow the agent. Arrows step the tab
to the agent's previous or next turn, found by paging its history, keeping the
arrangement and expanded items. Fills come from a fixed, validated categorical
palette in category order; text carries no series color.

`transcript_nav.rs` overlays the user's messages as ticks in the transcript's
top-right corner; hovering or focusing them lists previews. Choosing one moves
the window and publishes `data-jump-revision`/`data-jump-row`; the viewport
controller scrolls to that row once mounted and marks the navigator entry for
the current reading position. It lists the most recent loaded messages only.

Run headless checks with `cargo test -p psa_gui` and browser-controller checks with
`node --test client/psa_gui/src/*.test.cjs` from the workspace root. Tests cover the renderer-free
root with a fake platform, transcript HTML escaping, bounded attachment fallbacks,
split placement/drag geometry, server-scoped unavailable restored targets, and
draft/preview promotion. State/recovery reducers have further
tests in `client_shared`; backend protocol tests live with `psa_api`/`psa_server`.
