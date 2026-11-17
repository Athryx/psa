# client_shared

UI-independent state for PSA clients. Depends on the thin `psa_api` client, not a
UI framework or async runtime. The GUI owns cancellation and async subscription
tasks; reducers have no blocking I/O.

## Persistence

Serialize `DesktopSettings` to the platform's settings file. Connection keys are
plaintext secrets (the platform must apply restrictive permissions and atomic
writes). `Debug` redacts client keys. Favorites remain server-side model pins.
`ensure_current_server()` scopes layouts by endpoint; `Workspace::restore()`
repairs local layout invariants without discarding unavailable remote IDs.
Drafts, transcript caches, connections and view-local scroll state are not saved.

## Summary bootstrap

1. Cancel the old subscription, call `SessionIndex::begin_bootstrap()` and retain
   its generation token. Reconnect starts a **new** generation; do not replay
   ephemeral global revisions across connections.
2. Connect `Client::stream_global_events`, and apply its `Ready` message before
   fetching session pages. Apply each subsequent message using `apply_global`.
3. Fetch `Client::list_sessions` pages sequentially and call `merge_page` with the
   generation captured when starting each request. Per-session revisions prevent
   older snapshots from overwriting busy-only changes. Old-generation responses
   are ignored. Retain the cache when offline, but don't label cached state idle.
4. EOF, overflow, `ResyncRequired`, and reconnect all require another bootstrap.
   Backoff read/subscription retries; never silently retry message submissions.

Directory grouping uses the original remote path spelling. Groups include loaded
summaries only; `complete` remains false until pagination finishes.

## Transcript bootstrap

Fetch a recent page, call `Transcript::bootstrap`, then connect the same scope's
SSE stream with `after = transcript.cursor` and `include_pending = true`. The head
captured by HTTP closes the fetch/subscribe race through durable replay. Pass
messages in stream order to `apply`. For reconnect, retain caches and resume from
the last durable SSE ID, or the scope-local cursor; a stream snapshot replaces
pending events and queries. A resync signal requires reconnect/rebootstrap.

Older-page HTTP responses go to `merge_history`, **not** `bootstrap`: they cannot
replace live provisional state, questions, or the replay cursor. Do not mix agent
event IDs with session positions. The reducer deduplicates replay and retires
provisional IDs on commit/removal. Shared immutable payload maps keep snapshots
cheap; mutate them only through `bootstrap`, `merge_history` and `apply`, so the
display-rank and tool-result indexes remain consistent. `display_window` supports
a bounded viewport independent of loaded history, while raw `EventType` preserves
unsupported content for safe fallbacks. Tool correlation uses agent plus call ID,
including incomplete/orphan history.

The transcript also indexes user messages (`human_message_ids`), inference
starts and ends, and `NewContext` markers. `turn_number` counts a model turn,
by its inference start, from the user's preceding message; it is marked
inexact while older history is unloaded and no user message bounds the count.
Inference starts are display rows (the turn divider) only when
`UiPreferences::turn_dividers` is not `Hidden`; `inference_end` finds the end
a start opened. Successful ends are never display rows; failed ones always are.
`context_turn` resolves a row to the first inference whose context contained
it: input goes to the next inference to start, and a turn's own output to the
turn after it, since a context is everything before its inference's start. An
inference's start and end resolve to it; a row with no later started-and-ended
inference, or cut off by a `NewContext`, resolves to `None`.

`context.rs` reconstructs what one inference was sent from the events of its
context (fetch them with `EventSpan::ContextOf`). Providers report
only a total, so each item is weighed by text length and scaled to the reported
input tokens. `ContextMakeup::segments` arranges the diagram by order, size, or
type, with a bounded number of blocks for any context length.

Share one transcript/network subscription per scope across duplicate views.
Keep `FollowState` and UI expansion state per `ViewId`. `Drafts` is keyed by
`TranscriptTarget`; promote the editing view before changing a draft. Accepted
requests are not completed requests: `request_failures` is keyed by acceptance ID.

## Workspace

`open` searches the entire split tree and focuses the most recently focused
matching duplicate; it only replaces the focused group's preview when absent.
A tab's `TabKind` says what it shows of its target: the transcript, or one
turn's context. `open_context` reuses an agent's context tab, and `open` never
treats a context tab as the agent's transcript.
Use `promote` on double-click, draft edits, and submission. Independent `ViewId`s
are stable across serialization and never reused after closing tabs.
`open_duplicate` supports restored/future split views internally; ordinary
navigation should not call it. `split`, `move_tab`, and `close` preserve focus and
remove only the pane emptied by a move or close, leaving unrelated empty panes intact.
`close_active` also closes the focused empty pane, while preserving the last pane.
Splits expose ratios for UI resize controls.
