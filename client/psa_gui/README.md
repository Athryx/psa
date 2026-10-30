# psa_gui

Renderer-independent Dioxus 0.7.10 UI. This crate deliberately does **not** enable
Dioxus's desktop renderer. `psa_desktop` owns the WebView, asynchronous settings
loading, atomic secret-bearing persistence and the Tokio runtime.

Mount `App` with a `GuiBootstrap` root context containing `GuiConfig { settings }`
and `Rc<dyn GuiPlatform>`. `GuiPlatform::save_settings` returns a boxed async
future without a `Send` requirement, permitting thread-local web/mobile platform
implementations. Saves are serialized by the GUI and contain stable settings and
workspace IDs only, never transcript caches or drafts.

The UI uses `client_shared` workspace, session grouping, ancestry, transcript,
command parsing and reconnect policy. Network orchestration stays in `network.rs`:

- Subscribe to the global summary feed before fetching pages; revisions protect
  live upserts from late pages. Reconnect establishes a fresh ready boundary.
- Open views reference-count one live stream per session, including agent-tree
  updates. Session-position replay cursors are never confused with agent event IDs.
- Agent history is bounded initially, with explicit backward pagination. History
  merges do not replace live questions/provisional events.
- Server changes cancel old tasks and invalidate late HTTP responses. Mutating
  message submissions are never automatically retried; drafts survive failure.
- Provider-native/unsupported blocks have safe text fallbacks. No server content
  is inserted as raw HTML. Tool calls correlate results by call ID.

Tabs use previews, global focus-existing navigation, drag/reorder/move, four-edge
split drop zones, and draggable split dividers with arrow-key resizing. Keeping inactive tab views
mounted preserves their local scroll/expansion state during normal tab changes.
Workspace layout and sidebar state restore through the injected settings.

Run headless checks with `cargo test -p psa_gui`. Tests cover the renderer-free
root with a fake platform, transcript HTML escaping, bounded attachment fallbacks,
split placement/drag geometry, server-scoped unavailable restored targets, and
draft/preview promotion. State/recovery reducers have further
tests in `client_shared`; backend protocol tests live with `psa_api`/`psa_server`.
