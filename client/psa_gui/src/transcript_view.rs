//! View-local bounded transcript rendering. The reducer owns history; each pane
//! owns only a fixed-size window, follow state, and its browser scroll anchor.
use crate::{
    app::Ui,
    composer::{Composer, DraftComposer, Question},
    context_view::format_tokens,
    network,
    reactive::{Shared, use_ui_selector},
    tool_format::{build_code_preview, summarize_call},
    transcript_nav::{MessageNavigator, build_nav_entries},
};
use client_shared::*;
use dioxus::prelude::*;
use llm_api::api::ContentBlock;
use psa::api::*;
#[cfg(test)]
use std::collections::HashMap;
use std::{cell::RefCell, collections::HashSet, rc::Rc};

const WINDOW_ROWS: usize = 150;
const WINDOW_STEP: usize = WINDOW_ROWS / 2;
const LAZY_DETAIL_BYTES: usize = 4096;
const SUMMARY_PREVIEW_CHARS: usize = 160;

// Expansion belongs to a pane, not a mounted row. Recycled rows restore their
// state without retaining their event payloads; duplicate panes remain separate.
#[derive(Clone, Default)]
struct ExpansionState(Rc<RefCell<HashSet<(RowId, DetailMode)>>>);

pub(crate) fn update_following_from_scroll(mut following: Signal<bool>, near_bottom: bool) {
    // Redundant writes would rerun the follow effect and fight ordinary scrolling.
    if *following.peek() != near_bottom {
        following.set(near_bottom);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoryAction {
    Reveal,
    Fetch,
    None,
}

pub(crate) fn history_trigger(top: f64, active: bool, latched: &mut bool) -> bool {
    if top > 80.0 {
        *latched = false;
        return false;
    }
    if !active || *latched {
        return false;
    }
    *latched = true;
    true
}

pub(crate) fn history_action(
    count: usize,
    visible: usize,
    has_older: bool,
    busy: bool,
) -> HistoryAction {
    if count > visible {
        HistoryAction::Reveal
    } else if has_older && !busy {
        HistoryAction::Fetch
    } else {
        HistoryAction::None
    }
}

// Kept as the history-accounting contract for clients/tests. The renderer uses a
// bounded sliding window instead of turning this accounting limit into DOM rows.
#[cfg(test)]
fn updated_history_limit(
    limit: usize,
    previous: Option<(usize, Option<u64>)>,
    count: usize,
    boundary: Option<u64>,
    live_added: usize,
) -> usize {
    match previous {
        None => count.min(WINDOW_ROWS),
        Some((previous_count, old_boundary)) if old_boundary == boundary => {
            (limit + count.saturating_sub(previous_count)).min(count)
        }
        _ => (limit + live_added).min(count),
    }
}

pub(crate) fn history_needs_continuation(
    old_height: f64,
    new_height: f64,
    client_height: f64,
) -> bool {
    new_height <= client_height + 1.0 || new_height <= old_height + 1.0
}

pub(crate) fn restored_history_top(old_height: f64, old_top: f64, new_height: f64) -> f64 {
    (old_top + new_height - old_height).max(0.0)
}

pub(crate) fn scroll_transcript_to_bottom(element: Signal<Option<Rc<MountedData>>>) {
    if let Some(node) = element.peek().clone() {
        spawn(async move {
            let _ = node
                .scroll(
                    dioxus::html::geometry::PixelsVector2D::new(0.0, 1.0e9),
                    ScrollBehavior::Instant,
                )
                .await;
        });
    }
}

#[derive(Clone, PartialEq)]
struct ViewWindow {
    first: Option<RowId>,
    start: usize,
}

impl Default for ViewWindow {
    fn default() -> Self {
        Self {
            first: None,
            start: usize::MAX,
        }
    }
}

impl ViewWindow {
    fn start(
        &self,
        transcript: &Transcript,
        preferences: &UiPreferences,
        following: bool,
    ) -> usize {
        let maximum = transcript
            .display_count(preferences)
            .saturating_sub(WINDOW_ROWS);
        if following {
            return maximum;
        }
        self.first
            .as_ref()
            .and_then(|id| transcript.display_position(id, preferences))
            .unwrap_or(self.start)
            .min(maximum)
    }

    fn select(transcript: &Transcript, preferences: &UiPreferences, start: usize) -> Self {
        let start = start.min(
            transcript
                .display_count(preferences)
                .saturating_sub(WINDOW_ROWS),
        );
        Self {
            first: transcript.display_id_at(start, preferences),
            start,
        }
    }
}

#[derive(Clone, PartialEq)]
struct ViewSelection {
    transcript: Option<Shared<Transcript>>,
    preferences: UiPreferences,
    generation: u64,
    history_busy: bool,
    follow_request: (u64, Option<ViewId>),
}

#[derive(Clone, Default, PartialEq)]
struct ViewFrame {
    start: usize,
    count: usize,
    rows: Vec<RowPayload>,
}

fn row_key(id: &RowId) -> String {
    match id {
        RowId::Committed(id) => format!("c:{id}"),
        RowId::Pending(id) => format!("p:{id}"),
    }
}

#[derive(serde::Deserialize)]
struct ViewportMetrics {
    height: f64,
    top: f64,
    client_height: f64,
}

impl ViewportMetrics {
    fn as_tuple(&self) -> (f64, f64, f64) {
        (self.height, self.top, self.client_height)
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ViewportRequest {
    Follow {
        revision: u64,
        metrics: ViewportMetrics,
    },
    Window {
        start: usize,
        first: String,
        count: usize,
        history: bool,
        following: bool,
        follow_revision: u64,
        metrics: ViewportMetrics,
    },
    Scroll {
        following: bool,
        revision: u64,
        follow_revision: u64,
        metrics: ViewportMetrics,
    },
}

#[component]
pub(crate) fn TranscriptView(tab: Tab, active: bool) -> Element {
    #[cfg(test)]
    if let Some(counters) = try_use_context::<tests::RenderCounters>() {
        counters.record_view();
    }
    let mut ui = use_context::<Signal<Ui>>();
    use_context_provider(ExpansionState::default);
    let target = tab.target;
    let mut view_active = use_signal(|| active);
    use_effect(use_reactive((&active,), move |(active,)| {
        view_active.set(active)
    }));

    // A hidden pane retains its last immutable snapshot without subscribing to
    // new transcript payloads, preferences, history requests, or follow events.
    let mut saved = None::<ViewSelection>;
    let selection = use_ui_selector(move |state| {
        if view_active() || saved.is_none() {
            saved = Some(ViewSelection {
                transcript: view_active()
                    .then(|| state.transcripts.get(&target).cloned())
                    .flatten(),
                preferences: state.settings.preferences.clone(),
                generation: state.generation,
                history_busy: state.history_requests.get(&target) == Some(&state.generation),
                follow_request: state.follow_request,
            });
        }
        saved.as_ref().unwrap().clone()
    });
    let mut following = use_signal(|| selection.peek().preferences.auto_follow);
    let mut follow_revision = use_signal(|| 0_u64);
    let mut scroll_revision = use_signal(|| 0_u64);
    let mut window = use_signal(ViewWindow::default);
    let frame = use_memo(move || {
        let selected = selection.read();
        let Some(transcript) = selected.transcript.as_ref() else {
            return ViewFrame::default();
        };
        let start = window
            .read()
            .start(transcript, &selected.preferences, following());
        ViewFrame {
            start,
            count: transcript.display_count(&selected.preferences),
            rows: transcript.display_window(start, WINDOW_ROWS, &selected.preferences),
        }
    });
    // Once an initially empty transcript gets content, pin its first snapshot
    // as well; disabling follow must never silently turn into tail following.
    use_effect(move || {
        let current = frame.read();
        if window.peek().first.is_none() {
            if let Some(first) = current.rows.first() {
                window.set(ViewWindow {
                    first: Some(first.id()),
                    start: current.start,
                });
            }
        }
    });
    let nav_entries = use_memo(move || {
        let selected = selection.read();
        selected
            .transcript
            .as_ref()
            .map(|transcript| build_nav_entries(transcript, &selected.preferences))
            .unwrap_or_default()
    });
    // The browser controller scrolls to the requested row once it is mounted.
    let mut jump = use_signal(|| (0_u64, String::new()));
    let reveal = use_ui_selector(move |state| state.pending_reveals.get(&target).cloned());
    let mut element = use_signal(|| None::<Rc<MountedData>>);
    let mut controller = use_signal(|| None::<document::Eval>);
    let mut top_latched = use_signal(|| false);
    let mut scroll_metrics = use_signal(|| (0.0_f64, 0.0_f64, 0.0_f64));
    let mut restore = use_signal(|| None::<(f64, f64, f64)>);
    let mut load_request = use_signal(|| 0_u64);
    let mut load_steps = use_signal(|| 0_usize);
    let mut history_busy = use_signal(|| false);
    let mut history_waiting = use_signal(|| false);
    let mut history_error = use_signal(|| false);
    let mut history_auto_fill = use_signal(|| false);
    let dom_id = format!(
        "transcript-{}-{}-{}",
        tab.id.0, target.session_id.0, target.agent_id.0
    );
    let mount_id = dom_id.clone();
    use_drop(move || {
        if let Some(eval) = *controller.peek() {
            let _ = eval.send("dispose");
        }
    });

    // Browser-free renderers can still exercise history/follow behavior. Native
    // renderers use stable row anchors, not scroll-height deltas, when recycling.
    use_effect(move || {
        let Some((old_height, old_top, client_height)) = restore() else {
            return;
        };
        restore.set(None);
        if controller.peek().is_some() {
            history_busy.set(false);
            return;
        }
        let Some(node) = element.peek().clone() else {
            history_busy.set(false);
            return;
        };
        spawn(async move {
            if let Ok(size) = node.get_scroll_size().await {
                if (scroll_metrics.peek().1 - old_top).abs() > 1.0 || !*view_active.peek() {
                    history_busy.set(false);
                    return;
                }
                let top = if *history_auto_fill.peek() && *following.peek() {
                    (size.height - client_height).max(0.0)
                } else {
                    restored_history_top(old_height, old_top, size.height)
                };
                let _ = node
                    .scroll(
                        dioxus::html::geometry::PixelsVector2D::new(0.0, top),
                        ScrollBehavior::Instant,
                    )
                    .await;
                if history_needs_continuation(old_height, size.height, client_height) {
                    scroll_metrics.set((size.height, top, client_height));
                    load_steps += 1;
                    if *load_steps.peek() >= 10 {
                        tokio::task::yield_now().await;
                        load_steps.set(0);
                    }
                    load_request += 1;
                    return;
                }
            }
            history_busy.set(false);
        });
    });

    use_effect(move || {
        if load_request() == 0 || !*view_active.peek() {
            return;
        }
        let selected = selection.peek().clone();
        let Some(transcript) = selected.transcript.as_ref() else {
            history_busy.set(false);
            return;
        };
        let start = frame.peek().start;
        // Read the request registry at execution time: another pane's effect
        // may have claimed this target after this selector snapshot was made.
        let request_busy = {
            let state = ui.peek();
            state.history_requests.get(&target) == Some(&state.generation)
        };
        // "Visible" here counts the rows after the window's leading edge, not
        // the mounted rows. Cached history is revealed before a network request.
        match history_action(
            transcript.display_count(&selected.preferences),
            transcript
                .display_count(&selected.preferences)
                .saturating_sub(start),
            transcript.has_older,
            request_busy,
        ) {
            HistoryAction::Reveal => {
                if !*history_auto_fill.peek() {
                    following.set(false);
                }
                restore.set(Some(*scroll_metrics.peek()));
                window.set(ViewWindow::select(
                    transcript,
                    &selected.preferences,
                    start.saturating_sub(WINDOW_STEP),
                ));
            }
            HistoryAction::Fetch => {
                if !*history_auto_fill.peek() {
                    following.set(false);
                }
                let generation = selected.generation;
                let previous_count = transcript.row_count();
                let previous_boundary = transcript.next_before;
                ui.write().history_requests.insert(target, generation);
                dioxus::core::spawn_forever(async move {
                    let loaded = network::older(ui, target).await;
                    let mut state = ui.write();
                    if state.history_requests.get(&target) == Some(&generation) {
                        state.history_requests.remove(&target);
                    }
                    let current = state.transcripts.get(&target);
                    let progressed = current.is_some_and(|t| {
                        t.row_count() > previous_count
                            || t.next_before != previous_boundary
                            || !t.has_older
                    });
                    let valid = loaded && state.generation == generation && progressed;
                    let current = current.cloned();
                    drop(state);
                    if valid {
                        if let (Some(current), Ok(metrics), Ok(active)) =
                            (current, scroll_metrics.try_peek(), view_active.try_peek())
                        {
                            let metrics = *metrics;
                            if *active && metrics.1 <= 80.0 {
                                if let (Ok(mut next_window), Ok(mut anchor)) =
                                    (window.try_write(), restore.try_write())
                                {
                                    let preferences = &selection.peek().preferences;
                                    let start = next_window.start(&current, preferences, false);
                                    *next_window = ViewWindow::select(
                                        &current,
                                        preferences,
                                        start.saturating_sub(WINDOW_ROWS),
                                    );
                                    *anchor = Some(metrics);
                                    return;
                                }
                            }
                        }
                    } else {
                        if let Ok(mut error) = history_error.try_write() {
                            *error = true;
                        }
                        if let Ok(mut latch) = top_latched.try_write() {
                            *latch = false;
                        }
                    }
                    if let Ok(mut busy) = history_busy.try_write() {
                        *busy = false;
                    }
                });
            }
            HistoryAction::None => {
                if request_busy && transcript.has_older {
                    history_waiting.set(true);
                } else {
                    history_busy.set(false);
                }
            }
        }
    });

    use_effect(move || {
        let selected = selection.read();
        if !history_waiting() || selected.history_busy {
            return;
        }
        history_waiting.set(false);
        if *view_active.peek() && scroll_metrics.peek().1 <= 80.0 {
            load_request += 1;
        } else {
            history_busy.set(false);
        }
    });

    // Short initial panes (and pages made entirely of hidden events) cannot
    // generate a scroll gesture. Continue until there is content or exhaustion.
    use_effect(use_reactive((&active,), move |(active,)| {
        let node = element.read().clone();
        let current = frame.read();
        let selected = selection.read();
        let has_history =
            current.start > 0 || selected.transcript.as_ref().is_some_and(|t| t.has_older);
        if !active
            || !has_history
            || *history_busy.peek()
            || *history_error.peek()
            || controller.peek().is_some()
        {
            return;
        }
        if let Some(node) = node {
            spawn(async move {
                let (size, offset, rect) = (
                    node.get_scroll_size().await,
                    node.get_scroll_offset().await,
                    node.get_client_rect().await,
                );
                if let (Ok(size), Ok(offset), Ok(rect)) = (size, offset, rect) {
                    if *view_active.peek()
                        && size.height <= rect.size.height + 1.0
                        && !*history_busy.peek()
                    {
                        scroll_metrics.set((size.height, offset.y, rect.size.height));
                        history_auto_fill.set(true);
                        history_busy.set(true);
                        load_request += 1;
                    }
                }
            });
        }
    }));

    let mut seen_follow_request = use_signal(|| selection.peek().follow_request.0);
    use_effect(move || {
        let (revision, view) = selection.read().follow_request;
        if revision != *seen_follow_request.peek() {
            seen_follow_request.set(revision);
            if view == Some(tab.id) {
                follow_revision += 1;
                following.set(true);
                if controller.peek().is_none() {
                    scroll_transcript_to_bottom(element);
                }
            }
        }
    });
    use_effect(use_reactive((&active,), move |(active,)| {
        let _ = frame.read();
        let _ = element.read();
        if active && following() && controller.peek().is_none() {
            scroll_transcript_to_bottom(element);
        }
    }));

    let selected = selection.read();
    let current = frame.read();
    let preferences = selected.preferences.clone();
    let transcript = selected.transcript.as_ref();
    let queries = transcript
        .map(|t| t.queries.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let call_ids: std::collections::HashSet<_> = current
        .rows
        .iter()
        .filter_map(|payload| {
            if let psa::api::events::EventType::ToolCall(call) = payload.kind() {
                Some((payload.agent_id(), call.call_id.clone()))
            } else {
                None
            }
        })
        .collect();
    let visible: Vec<_> = current.rows.iter().enumerate().filter(|(_, payload)| {
        !matches!(payload.kind(), psa::api::events::EventType::ToolResult(result) if call_ids.contains(&(payload.agent_id(), result.call_id.clone())))
            && row_is_visible(payload.kind(), &preferences)
    }).collect();
    let row_preferences = RowPreferences::from(&preferences);
    let layouts = layout_rows(
        visible
            .iter()
            .map(|(_, payload)| (payload.agent_id(), payload.kind())),
        &row_preferences,
    );
    let rows: Vec<_> = visible
        .into_iter()
        .zip(layouts)
        .map(|((offset, payload), layout)| {
            // A tool call carries its result, and a turn's start its end.
            let result = match payload.kind() {
                psa::api::events::EventType::ToolCall(call) => {
                    transcript.and_then(|t| t.tool_result(payload.agent_id(), &call.call_id))
                }
                psa::api::events::EventType::InferenceResultStart(_) => {
                    transcript.and_then(|t| t.inference_end(&payload.id()))
                }
                _ => None,
            };
            let turn = transcript
                .filter(|_| row_preferences.turn_dividers == TurnDividerDisplay::Labeled)
                .and_then(|t| t.turn_number(&payload.id()));
            (
                current.start + offset,
                payload.id(),
                row_key(&payload.id()),
                payload.clone(),
                result,
                layout,
                turn,
            )
        })
        .collect();
    let first = current
        .rows
        .first()
        .map(|row| row_key(&row.id()))
        .unwrap_or_default();
    let end = current.start + current.rows.len();
    let count = current.count;
    let start = current.start;
    let has_older = transcript.is_some_and(|t| t.has_older);
    let font_size = preferences.transcript_font_size;
    let max_width = preferences.conversation_max_width_ch;
    let mut jump_to_row = move |id: RowId| {
        let selected = selection.peek();
        let Some(transcript) = selected.transcript.as_ref() else {
            return;
        };
        let Some(position) = transcript.display_position(&id, &selected.preferences) else {
            return;
        };

        // An explicit revision makes leaving follow authoritative over
        // any wheel acknowledgement still in flight from the browser.
        window.set(ViewWindow::select(
            transcript,
            &selected.preferences,
            position.saturating_sub(WINDOW_ROWS / 2),
        ));
        history_auto_fill.set(false);
        follow_revision += 1;
        following.set(false);
        let revision = jump.peek().0 + 1;
        jump.set((revision, row_key(&id)));
    };
    // A linked message is revealed once this transcript's first page arrives.
    use_effect(move || {
        let Some(link) = reveal() else {
            return;
        };
        let selected = selection.read();
        let Some(transcript) = selected.transcript.as_ref().filter(|t| t.row_count() > 0) else {
            return;
        };
        let found = link.find(transcript);
        drop(selected);
        ui.write().pending_reveals.remove(&target);
        match (found, &link) {
            (Some(id), _) => jump_to_row(id),
            // Completion reports have no sending call: show the latest activity.
            (None, MessageLink::Sent { .. }) => {
                follow_revision += 1;
                following.set(true);
            }
            (None, _) => {
                ui.write().notice = Some(
                    "That message is outside the loaded history; scroll up to load older events."
                        .into(),
                )
            }
        }
    });
    let (jump_revision, jump_row) = jump();
    let mut row_menu = use_signal(|| None::<RowMenu>);
    rsx! {
        div { class: "transcript-frame",
        div {
            id: dom_id, class: "transcript", "data-window-start": start, "data-window-end": end,
            "data-row-count": count, "data-first-row": first, "data-active": active,
            "data-following": following(), "data-follow-revision": follow_revision(),
            "data-scroll-revision": scroll_revision(), "data-has-older": has_older, "data-history-busy": history_busy(),
            "data-jump-revision": jump_revision, "data-jump-row": jump_row,
            style: "font-size:var(--psa-transcript-font-size,{font_size}px);--conversation-max-width:var(--psa-conversation-width,{max_width}rch);overflow-anchor:none",
            onmounted: move |event| {
                element.set(Some(event.data()));
                let id = mount_id.clone();
                spawn(async move {
                    let mut eval = document::eval(&format!("{}\n{}", include_str!("transcript_viewport.js"), include_str!("transcript_viewport_bridge.js")));
                    if eval.send(serde_json::json!({"id": id, "windowRows": WINDOW_ROWS})).is_err() { return; }
                    controller.set(Some(eval));
                    while let Ok(request) = eval.recv::<ViewportRequest>().await {
                        if !*view_active.peek() { continue; }
                        match request {
                            ViewportRequest::Follow { revision, metrics } => {
                                if revision <= *follow_revision.peek() { continue; }
                                follow_revision.set(revision);
                                scroll_metrics.set(metrics.as_tuple());
                                update_following_from_scroll(following, true);
                            }
                            ViewportRequest::Scroll { following: next, revision, follow_revision: requested_follow, metrics } => {
                                // A delayed wheel acknowledgement cannot cancel a
                                // newer explicit Follow/Ctrl-End request.
                                if requested_follow != *follow_revision.peek() { continue; }
                                scroll_metrics.set(metrics.as_tuple());
                                if *scroll_revision.peek() != revision { scroll_revision.set(revision); }
                                if !next {
                                    if *following.peek() {
                                        let selected = selection.peek();
                                        if let Some(transcript) = selected.transcript.as_ref() {
                                            window.set(ViewWindow::select(transcript, &selected.preferences, frame.peek().start));
                                        }
                                    }
                                    history_auto_fill.set(false);
                                }
                                update_following_from_scroll(following, next);
                            }
                            ViewportRequest::Window { start, first, count, history, following: local_following, follow_revision: requested_follow, metrics } => {
                                if requested_follow != *follow_revision.peek() { continue; }
                                let current = frame.peek();
                                let current_first = current.rows.first().map(|row| row_key(&row.id())).unwrap_or_default();
                                if first != current_first || count != current.count { continue; }
                                drop(current);
                                if history {
                                    if !*history_busy.peek() && !*history_error.peek() {
                                        history_auto_fill.set(local_following || metrics.height <= metrics.client_height + 1.0);
                                        scroll_metrics.set(metrics.as_tuple());
                                        history_busy.set(true);
                                        load_request += 1;
                                    }
                                } else if let Some(transcript) = selection.peek().transcript.as_ref() {
                                    let next = ViewWindow::select(transcript, &selection.peek().preferences, start);
                                    if *window.peek() != next { window.set(next); }
                                }
                            }
                        }
                    }
                    controller.set(None);
                    history_busy.set(false);
                });
            },
            onscroll: move |event| {
                // Browser capture owns native scrolling; retain this only for
                // browser-free renderers and their synthetic event tests.
                if !active || controller.peek().is_some() { return; }
                let top = event.scroll_top();
                let height = event.scroll_height() as f64;
                scroll_metrics.set((height, top, event.client_height() as f64));
                let near_bottom = height - event.client_height() as f64 - top < 80.0;
                if !near_bottom {
                    if *following.peek() {
                        let selected = selection.peek();
                        if let Some(transcript) = selected.transcript.as_ref() {
                            window.set(ViewWindow::select(transcript, &selected.preferences, frame.peek().start));
                        }
                    }
                    history_auto_fill.set(false);
                    update_following_from_scroll(following, false);
                } else if !*history_busy.peek() && (top > 80.0 || !*top_latched.peek()) {
                    update_following_from_scroll(following, true);
                }
                let mut latched = *top_latched.peek();
                let short_follow = *following.peek() && height <= event.client_height() as f64 + 1.0;
                let trigger = !short_follow && history_trigger(top, active, &mut latched);
                if latched != *top_latched.peek() { top_latched.set(latched); }
                if trigger && !*history_busy.peek() {
                    history_auto_fill.set(false);
                    history_error.set(false);
                    history_busy.set(true);
                    load_steps.set(0);
                    load_request += 1;
                }
            },
            onwheel: move |event| {
                let upward = event.delta().strip_units().y < 0.0;
                if !active || !upward || controller.peek().is_some() { return; }
                if *history_busy.peek() { history_auto_fill.set(false); following.set(false); return; }
                if let Some(node) = element.peek().clone() {
                    spawn(async move {
                        let (size, offset, rect) = (node.get_scroll_size().await, node.get_scroll_offset().await, node.get_client_rect().await);
                        if let (Ok(size), Ok(offset), Ok(rect)) = (size, offset, rect) {
                            if *view_active.peek() && offset.y <= 80.0 && !*history_busy.peek() {
                                scroll_metrics.set((size.height, offset.y, rect.size.height));
                                top_latched.set(true);
                                history_auto_fill.set(false);
                                history_error.set(false);
                                history_busy.set(true);
                                load_request += 1;
                            }
                        }
                    });
                }
            },
            div { "data-transcript-spacer": "top", style: "height:0px;margin:0;flex-shrink:0", "aria-hidden": "true" }
            if history_error() {
                button { onclick: move |_| { history_auto_fill.set(false); history_error.set(false); history_busy.set(true); load_request += 1; }, "Retry loading history" }
            }
            for (index, id, key, payload, result, layout, turn) in rows {
                div { key: "{key}", class: layout.class(), "data-transcript-row": key, "data-row-index": index,
                    "data-row-gap": layout.spacing.gap_attribute(),
                    oncontextmenu: move |event: MouseEvent| {
                        event.prevent_default();
                        let point = event.client_coordinates();
                        let turn = selection.peek().transcript.as_ref().and_then(|t| t.context_turn(&id));
                        row_menu.set(Some(RowMenu { x: point.x, y: point.y, turn }));
                    },
                    CachedEventRow { payload, result, preferences: row_preferences.clone(), group: layout.group, turn }
                }
            }
            div { "data-transcript-spacer": "bottom", style: "height:0px;margin:0;flex-shrink:0", "aria-hidden": "true" }
            for query in queries { Question { key: "{query.id}", query } }
            if !following() { button { class: "new-activity", onclick: move |_| {
                follow_revision += 1;
                following.set(true);
                if controller.peek().is_none() { scroll_transcript_to_bottom(element); }
            }, "Follow new activity ↓" } }
        }
        MessageNavigator { entries: nav_entries, onjump: move |id: RowId| jump_to_row(id) }
        if let Some(menu) = row_menu() {
            RowContextMenu { menu, target, onclose: move |_| row_menu.set(None) }
        }
        }
        if target.agent_id == crate::app::DRAFT_ROOT { DraftComposer { tab } } else { Composer { tab } }
    }
}

fn row_is_visible(kind: &psa::api::events::EventType, preferences: &UiPreferences) -> bool {
    use psa::api::events::{EventType, MessageType};
    match kind {
        EventType::Message(message) if message.message_type == MessageType::Harness => {
            preferences.show_harness_messages
        }
        EventType::Log(log) => preferences.log_level.shows(log.error),
        EventType::Message(_)
        | EventType::Command(_)
        | EventType::DecisionCall(_)
        | EventType::ToolCall(_)
        | EventType::ToolResult(_)
        | EventType::Compaction(_)
        | EventType::ProviderItem(_) => true,
        EventType::Thinking(thinking) => {
            preferences.reasoning_display != ReasoningDisplay::Hidden
                && thinking
                    .contents
                    .summary
                    .iter()
                    .any(|text| !text.is_empty())
        }
        EventType::Result(result) => {
            !preferences.hide_text_recorded_results || !matches!(result, AgentResult::Text(_))
        }
        EventType::InferenceResultStart(_) => {
            preferences.turn_dividers != TurnDividerDisplay::Hidden
        }
        EventType::InferenceResultEnd(end) => !is_successful_inference(end),
        EventType::StatusChange(status) => {
            status.new_status == AgentState::Interrupted || !preferences.hide_agent_statuses
        }
        _ => false,
    }
}

#[derive(Clone, PartialEq)]
struct RowPreferences {
    reasoning_display: ReasoningDisplay,
    hide_text_recorded_results: bool,
    hide_agent_statuses: bool,
    turn_dividers: TurnDividerDisplay,
}
impl From<&UiPreferences> for RowPreferences {
    fn from(preferences: &UiPreferences) -> Self {
        Self {
            reasoning_display: preferences.reasoning_display,
            hide_text_recorded_results: preferences.hide_text_recorded_results,
            hide_agent_statuses: preferences.hide_agent_statuses,
            turn_dividers: preferences.turn_dividers,
        }
    }
}

/// Where a compact row sits within a run of consecutive rows sharing a label.
/// The first row of a run carries the label; the others are indented below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GroupPosition {
    #[default]
    Single,
    First,
    Middle,
    Last,
}

impl GroupPosition {
    fn is_grouped(self) -> bool {
        self != Self::Single
    }
}

#[derive(PartialEq, Eq)]
enum GroupKey<'a> {
    SystemPrompt,
    Harness,
    Decision,
    Reasoning,
    Tool(&'a str),
}

fn group_key<'a>(
    kind: &'a psa::api::events::EventType,
    preferences: &RowPreferences,
) -> Option<GroupKey<'a>> {
    use psa::api::events::{EventType, MessageType};
    match kind {
        EventType::Message(message) if message.message_type == MessageType::System => {
            Some(GroupKey::SystemPrompt)
        }
        EventType::Message(message) if message.message_type == MessageType::Harness => {
            Some(GroupKey::Harness)
        }
        EventType::DecisionCall(_) => Some(GroupKey::Decision),
        EventType::ToolCall(call) => Some(GroupKey::Tool(&call.tool_name)),
        EventType::Thinking(_) if preferences.reasoning_display == ReasoningDisplay::Collapsed => {
            Some(GroupKey::Reasoning)
        }
        _ => None,
    }
}

/// Rows drawn as one expandable line, which sit closer together than messages.
fn is_compact_row(kind: &psa::api::events::EventType, preferences: &RowPreferences) -> bool {
    use psa::api::events::EventType;
    match kind {
        EventType::ToolResult(_)
        | EventType::Log(_)
        | EventType::Command(_)
        | EventType::StatusChange(_)
        | EventType::InferenceResultEnd(_)
        | EventType::Compaction(_)
        | EventType::ProviderItem(_)
        | EventType::Result(_) => true,
        EventType::Message(message)
            if message.message_type == psa::api::events::MessageType::Subagent =>
        {
            true
        }
        _ => group_key(kind, preferences).is_some(),
    }
}

/// The gap above a row. The viewport controller reads the same gap from
/// `data-row-gap` when it estimates the height of recycled rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RowSpacing {
    #[default]
    Normal,
    /// Beside a turn divider, which separates rows by itself.
    Near,
    /// A compact row following another, with no gap between the two.
    Tight,
}

impl RowSpacing {
    fn gap_attribute(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::Near => Some("6"),
            Self::Tight => Some("0"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowLayout {
    group: GroupPosition,
    compact: bool,
    spacing: RowSpacing,
}

impl RowLayout {
    fn class(self) -> &'static str {
        match (self.compact, self.spacing) {
            (true, RowSpacing::Tight) => "row compact tight",
            (true, RowSpacing::Near) => "row compact near",
            (true, RowSpacing::Normal) => "row compact",
            (false, RowSpacing::Normal) => "row",
            (false, _) => "row near",
        }
    }
}

/// Lay out the mounted rows only: a run cut by the window edge simply restarts.
fn layout_rows<'a>(
    rows: impl Iterator<Item = (AgentId, &'a psa::api::events::EventType)>,
    preferences: &RowPreferences,
) -> Vec<RowLayout> {
    let rows: Vec<_> = rows
        .map(|(agent_id, kind)| {
            let key = group_key(kind, preferences).map(|key| (agent_id, key));
            let divides = matches!(kind, psa::api::events::EventType::InferenceResultStart(_));
            (key, is_compact_row(kind, preferences), divides)
        })
        .collect();

    (0..rows.len())
        .map(|index| {
            let (key, compact, divides) = &rows[index];
            let joins = |other: Option<&(Option<(AgentId, GroupKey)>, bool, bool)>| {
                key.is_some() && other.is_some_and(|(other, ..)| other == key)
            };
            let previous = index.checked_sub(1).and_then(|index| rows.get(index));
            let group = match (joins(previous), joins(rows.get(index + 1))) {
                (false, false) => GroupPosition::Single,
                (false, true) => GroupPosition::First,
                (true, true) => GroupPosition::Middle,
                (true, false) => GroupPosition::Last,
            };
            let spacing = match previous {
                Some((_, true, _)) if *compact => RowSpacing::Tight,
                Some((.., previous_divides)) if *divides || *previous_divides => RowSpacing::Near,
                _ => RowSpacing::Normal,
            };
            RowLayout {
                group,
                compact: *compact,
                spacing,
            }
        })
        .collect()
}

#[component]
fn CachedEventRow(
    payload: RowPayload,
    result: Option<RowPayload>,
    preferences: RowPreferences,
    #[props(default)] group: GroupPosition,
    #[props(default)] turn: Option<TurnNumber>,
) -> Element {
    #[cfg(test)]
    if let Some(counters) = try_use_context::<tests::RenderCounters>() {
        counters.record_row(payload.id());
    }
    rsx! { EventContent { kind: EventKind::Payload(payload), result: result.map(EventKind::Payload), preferences, group, turn } }
}

#[derive(Clone)]
enum EventKind {
    Payload(RowPayload),
    #[cfg(test)]
    Owned(Shared<psa::api::events::EventType>),
}
impl PartialEq for EventKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Payload(left), Self::Payload(right)) => left == right,
            #[cfg(test)]
            (Self::Owned(left), Self::Owned(right)) => left == right,
            #[cfg(test)]
            _ => false,
        }
    }
}
impl EventKind {
    fn kind(&self) -> &psa::api::events::EventType {
        match self {
            Self::Payload(payload) => payload.kind(),
            #[cfg(test)]
            Self::Owned(kind) => kind,
        }
    }
    fn is_provisional(&self) -> bool {
        matches!(self, Self::Payload(payload) if payload.is_provisional())
    }
    fn row_id(&self) -> Option<RowId> {
        match self {
            Self::Payload(payload) => Some(payload.id()),
            #[cfg(test)]
            Self::Owned(_) => None,
        }
    }
    /// The agent and event ID naming a committed inference to a context view.
    fn context_turn(&self) -> Option<(TranscriptTarget, u64)> {
        match self {
            Self::Payload(RowPayload::Committed(_, envelope)) => Some((
                TranscriptTarget {
                    session_id: envelope.session_id,
                    agent_id: envelope.agent_id,
                },
                envelope.event.id.0,
            )),
            _ => None,
        }
    }
}

// Compatibility for standalone row renderers/tests. Actual transcript rows use
// CachedEventRow above, with no context access and no global Ui subscription.
#[cfg(test)]
#[derive(Props, Clone)]
pub(crate) struct EventRowProps {
    kind: psa::api::events::EventType,
    provisional: bool,
    results: Rc<HashMap<String, psa::api::events::ToolResult>>,
}
#[cfg(test)]
impl PartialEq for EventRowProps {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(&self.kind, &other.kind)
            && self.provisional == other.provisional
            && Rc::ptr_eq(&self.results, &other.results)
    }
}
#[cfg(test)]
#[allow(non_snake_case)]
pub(crate) fn EventRow(props: EventRowProps) -> Element {
    let ui = try_use_context::<Signal<Ui>>();
    let preferences = use_memo(move || {
        ui.map(|ui| ui.read().settings.preferences.clone())
            .unwrap_or_default()
    });
    let result = if let psa::api::events::EventType::ToolCall(call) = &props.kind {
        props
            .results
            .get(&call.call_id)
            .cloned()
            .map(psa::api::events::EventType::ToolResult)
            .map(Shared::new)
            .map(EventKind::Owned)
    } else {
        None
    };
    rsx! { EventContent { kind: EventKind::Owned(Shared::new(props.kind)), result, preferences: RowPreferences::from(&preferences()), provisional: props.provisional } }
}

pub(crate) fn blocks(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Refusal(refusal) => refusal.text.clone(),
            ContentBlock::Image(_) => "[Image attachment]".into(),
            ContentBlock::Audio(audio) => audio
                .transcript
                .clone()
                .unwrap_or_else(|| "[Audio attachment]".into()),
            ContentBlock::File(file) => format!(
                "[File attachment: {}]",
                file.filename.as_deref().unwrap_or("unnamed")
            ),
            ContentBlock::ProviderContent(content) => format!(
                "[Provider content: {} / {}]",
                content.provider.as_str(),
                content.type_name().unwrap_or("unknown")
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn blocks_are_large(content: &[ContentBlock]) -> bool {
    let mut bytes = 0_usize;
    for block in content {
        bytes = bytes.saturating_add(32).saturating_add(match block {
            ContentBlock::Text(text) => text.text.len(),
            ContentBlock::Refusal(refusal) => refusal.text.len(),
            ContentBlock::Audio(audio) => audio.transcript.as_ref().map_or(0, String::len),
            _ => 100,
        });
        if bytes > LAZY_DETAIL_BYTES {
            return true;
        }
    }
    false
}

fn text_parts_are_large(parts: &[String]) -> bool {
    let mut bytes = 0_usize;
    parts.iter().any(|text| {
        bytes = bytes.saturating_add(text.len()).saturating_add(1);
        bytes > LAZY_DETAIL_BYTES
    })
}

fn json_is_large(value: &serde_json::Value, budget: &mut usize) -> bool {
    // Stop at the budget; deciding whether to serialize must not serialize or
    // traverse an entire enormous provider payload in the collapsed path.
    let cost = match value {
        serde_json::Value::String(text) => text.len(),
        _ => 8,
    };
    if cost > *budget {
        return true;
    }
    *budget -= cost;
    match value {
        serde_json::Value::Array(items) => items.iter().any(|item| json_is_large(item, budget)),
        serde_json::Value::Object(items) => items.iter().any(|(key, item)| {
            if key.len() > *budget {
                return true;
            }
            *budget -= key.len();
            json_is_large(item, budget)
        }),
        _ => false,
    }
}

fn error_details_are_large(error: &llm_api::Error) -> bool {
    let llm_api::Error::Provider {
        code,
        message,
        metadata,
    } = error
    else {
        return false;
    };
    let text_bytes = message
        .len()
        .saturating_add(code.as_ref().map_or(0, String::len));
    if text_bytes > LAZY_DETAIL_BYTES {
        return true;
    }
    let mut budget = LAZY_DETAIL_BYTES - text_bytes;
    metadata.iter().any(|(key, value)| {
        if key.len() > budget {
            return true;
        }
        budget -= key.len();
        json_is_large(value, &mut budget)
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum DetailMode {
    System,
    Harness,
    Log,
    Decision,
    Command,
    Tool,
    Unmatched,
    Compaction,
    Provider,
    Thinking,
    Result,
    Error,
    Received,
}

/// An agent's name that opens its transcript at a linked message. Rows have no
/// Ui subscription; the context is only read when clicked.
#[component]
fn AgentLink(
    session_id: SessionId,
    agent_id: AgentId,
    label: String,
    link: MessageLink,
) -> Element {
    let ui = try_use_context::<Signal<Ui>>();
    rsx! {
        button { r#type: "button", class: "text-link agent-link", title: "Open {label} at this message",
            onclick: move |event| {
                // Inside a row summary: never toggle the row itself.
                event.prevent_default();
                event.stop_propagation();
                if let Some(mut ui) = ui {
                    ui.write().reveal_message(TranscriptTarget { session_id, agent_id }, link.clone());
                }
            },
            "{label}"
        }
    }
}

/// Text blocks rendered as Markdown, as in agent messages.
fn message_markdown(content: &[ContentBlock]) -> Element {
    rsx! {
        for block in content {
            match block {
                ContentBlock::Text(text) => rsx! { crate::markdown::Markdown { text: text.text.clone() } },
                _ => rsx! { pre { "{blocks(std::slice::from_ref(block))}" } },
            }
        }
    }
}

/// An expandable line. Its children form the always-visible summary.
#[component]
fn EventDetails(
    kind: EventKind,
    result: Option<EventKind>,
    mode: DetailMode,
    class: String,
    lazy: bool,
    children: Element,
) -> Element {
    let persistence = try_use_context::<ExpansionState>();
    let key = kind.row_id().map(|id| (id, mode));
    let initial_persistence = persistence.clone();
    let initial_key = key.clone();
    let mut expanded = use_signal(move || {
        initial_key.as_ref().is_some_and(|key| {
            initial_persistence
                .as_ref()
                .is_some_and(|state| state.0.borrow().contains(key))
        })
    });
    let synchronized_persistence = persistence.clone();
    let mut seen_key = use_signal(|| key.clone());
    use_effect(use_reactive((&key,), move |(key,)| {
        if *seen_key.peek() == key {
            return;
        }
        seen_key.set(key.clone());
        let restored = key.as_ref().is_some_and(|key| {
            synchronized_persistence
                .as_ref()
                .is_some_and(|state| state.0.borrow().contains(key))
        });
        if *expanded.peek() != restored {
            expanded.set(restored);
        }
    }));
    rsx! {
        details { class, open: expanded(),
            summary { onclick: move |event| {
                event.prevent_default();
                let next = !*expanded.peek();
                expanded.set(next);
                if let (Some(state), Some(key)) = (&persistence, &key) {
                    if next { state.0.borrow_mut().insert(key.clone()); }
                    else { state.0.borrow_mut().remove(key); }
                }
            }, {children} }
            if expanded() || !lazy {
                div { class: "item-body", DetailBody { kind, result, mode } }
            }
        }
    }
}

#[component]
fn DetailBody(kind: EventKind, result: Option<EventKind>, mode: DetailMode) -> Element {
    use psa::api::events::EventType;
    match (mode, kind.kind()) {
        (DetailMode::Log, EventType::Log(log)) => rsx! { pre { "{log.message}" } },
        (DetailMode::Decision, EventType::DecisionCall(call)) => {
            rsx! { pre { "{client_shared::describe_decision_call(call)}" } }
        }
        (DetailMode::Command, EventType::Command(command)) => {
            rsx! { pre { "{command.output.clone().unwrap_or_default()}" } }
        }
        (DetailMode::System | DetailMode::Harness, EventType::Message(message)) => {
            rsx! { pre { "{blocks(&message.content)}" } }
        }
        (DetailMode::Received, EventType::Message(message)) => message_markdown(&message.content),
        (DetailMode::Tool, EventType::ToolCall(call)) => {
            rsx! {
                ToolArguments { tool_name: call.tool_name.clone(), arguments: call.arguments.clone() }
                if let Some(EventType::ToolResult(result)) = result.as_ref().map(EventKind::kind) {
                    h4 { if result.error { "Error" } else { "Result" } }
                    pre { "{blocks(&result.content)}" }
                }
            }
        }
        (DetailMode::Unmatched, EventType::ToolResult(result)) => {
            rsx! { pre { "{blocks(&result.content)}" } }
        }
        (DetailMode::Compaction, EventType::Compaction(compaction)) => {
            rsx! { pre { "{serde_json::to_string_pretty(compaction).unwrap_or_default()}" } }
        }
        (DetailMode::Provider, EventType::ProviderItem(item)) => {
            rsx! { pre { "{serde_json::to_string_pretty(item).unwrap_or_default()}" } }
        }
        (DetailMode::Thinking, EventType::Thinking(thinking)) => {
            rsx! { pre { "{thinking.contents.summary.join(\"\n\")}" } }
        }
        (DetailMode::Result, EventType::Result(result)) => {
            rsx! { pre { "{serde_json::to_string_pretty(result).unwrap_or_default()}" } }
        }
        (DetailMode::Error, EventType::InferenceResultEnd(end)) => match &end.outcome {
            llm_api::api::Outcome::Failed(error) => rsx! {
                pre { "{error}" }
                h4 { "Error details" }
                pre { "{serde_json::to_string_pretty(error).unwrap_or_default()}" }
            },
            _ => rsx! {},
        },
        _ => rsx! {},
    }
}

/// A tool call's arguments, with any code among them highlighted.
#[component]
pub(crate) fn ToolArguments(tool_name: String, arguments: String) -> Element {
    let arguments = crate::tool_format::format_arguments(&tool_name, &arguments);
    rsx! {
        if let Some((code, language)) = arguments.code {
            h4 { if language == "bash" { "Command" } else { "Code" } }
            crate::syntax_highlight::HighlightedCode { code, language: code_language(language) }
        }
        if !arguments.kwargs.is_empty() { h4 { "Arguments" } pre { "{arguments.kwargs}" } }
        for argument in arguments.text {
            h4 { class: "item-argument", "{argument.name}" }
            pre { "{argument.text}" }
        }
    }
}

fn code_language(name: &str) -> crate::syntax_highlight::CodeLanguage {
    match name {
        "bash" => crate::syntax_highlight::CodeLanguage::Bash,
        _ => crate::syntax_highlight::CodeLanguage::Python,
    }
}

/// The first non-blank line, bounded for a single-line summary. Markdown
/// emphasis around a leading title is dropped, as the summary is plain text.
fn build_line_preview<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let line = parts
        .into_iter()
        .flat_map(str::lines)
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || "*#`_".contains(c)))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(SUMMARY_PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    format!(
        "{}m {:02}s",
        duration.as_secs() / 60,
        duration.as_secs() % 60
    )
}

/// A row's right-click menu, at viewport coordinates. `turn` is the inference
/// the row belongs to, once that inference has ended.
#[derive(Clone, Copy, PartialEq)]
struct RowMenu {
    x: f64,
    y: f64,
    turn: Option<u64>,
}

#[component]
fn RowContextMenu(menu: RowMenu, target: TranscriptTarget, onclose: EventHandler<()>) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let RowMenu { x, y, turn } = menu;
    rsx! {
        div {
            class: "row-menu-backdrop",
            onclick: move |_| onclose.call(()),
            oncontextmenu: move |event| { event.prevent_default(); onclose.call(()); },
        }
        div {
            class: "row-menu", role: "menu", tabindex: "-1",
            style: "left:min({x}px,calc(100vw - 168px));top:min({y}px,calc(100vh - 48px))",
            onmounted: move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
            onkeydown: move |event| {
                if event.key() == Key::Escape {
                    event.prevent_default();
                    event.stop_propagation();
                    onclose.call(());
                }
            },
            button {
                r#type: "button", role: "menuitem", disabled: turn.is_none(),
                title: if turn.is_some() { "Show the first context this was sent to the model in" } else { "Not yet in any finished model turn's context" },
                onclick: move |_| {
                    if let Some(turn) = turn {
                        ui.write().open_context(target, turn);
                    }
                    onclose.call(());
                },
                "Show context"
            }
        }
    }
}

/// The start of one model turn, between what it was sent (everything above)
/// and what it produced (below): its number, figures from the turn's end, and
/// a way to inspect what its context was made of. `end` is `None` while the
/// turn is still running.
#[component]
fn TurnDivider(end: Option<EventKind>, turn: Option<TurnNumber>) -> Element {
    let ui = try_use_context::<Signal<Ui>>();
    let context_turn = end.as_ref().and_then(EventKind::context_turn);

    let details = match end.as_ref().map(EventKind::kind) {
        Some(psa::api::events::EventType::InferenceResultEnd(end)) => {
            let mut details = Vec::new();
            if let Some(tokens) = end.usage.input_tokens {
                details.push(format!("{} context", format_tokens(tokens)));
            }
            if let Some(tokens) = end.usage.output_tokens {
                details.push(format!("{} out", format_tokens(tokens)));
            }
            if !end.request_time.is_zero() {
                details.push(format_duration(end.request_time));
            }
            details.join(" · ")
        }
        _ => "running…".to_owned(),
    };
    rsx! {
        div { class: "turn-divider", role: "separator",
            span { class: "turn-label",
                if let Some(turn) = turn {
                    span {
                        class: "turn-number",
                        title: if turn.exact { "Model calls since your last message" } else { "Model calls in the loaded history; earlier history is not loaded" },
                        "turn {turn.number}" if !turn.exact { "+" }
                    }
                }
                if turn.is_some() && !details.is_empty() { " · " }
                "{details}"
            }
            span { class: "turn-rule" }
            if let (Some(mut ui), Some((target, turn))) = (ui, context_turn) {
                button {
                    class: "turn-context", r#type: "button",
                    title: "Show what this turn was sent: everything above this line",
                    onclick: move |_| ui.write().open_context(target, turn),
                    "context"
                }
            }
        }
    }
}

#[component]
fn EventContent(
    kind: EventKind,
    result: Option<EventKind>,
    preferences: RowPreferences,
    #[props(default)] provisional: bool,
    #[props(default)] group: GroupPosition,
    #[props(default)] turn: Option<TurnNumber>,
) -> Element {
    use psa::api::events::EventType;
    let provisional = provisional || kind.is_provisional();
    // A compact line: its label heads a run of like rows, or else the line itself.
    let compact = |mode, label: &str, class: &str, lazy, summary: Element| {
        let class = if group.is_grouped() {
            format!("item grouped {class}")
        } else {
            format!("item {class}")
        };
        rsx! {
            if group == GroupPosition::First {
                div { class: if mode == DetailMode::Tool { "group-label tool" } else { "group-label" }, "{label}" }
            }
            EventDetails { kind: kind.clone(), result: result.clone(), mode, class, lazy,
                span { class: "item-label", "{label}" }
                {summary}
            }
        }
    };
    // A labelled line with nothing to expand.
    let line = |label: &str, class: &str, text: String| {
        let class = format!("item item-line {class}");
        let class = class.trim_end();
        rsx! { div { class, span { class: "item-label", "{label}" } span { class: "item-text", "{text}" } } }
    };
    match kind.kind() {
        EventType::Message(message) => {
            if matches!(message.message_type, psa::api::events::MessageType::System) {
                // The body stays unbuilt while collapsed; an undescribed prompt
                // is told apart by its size alone.
                let description = message.description.clone().unwrap_or_else(|| {
                    let characters: usize = message
                        .content
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Text(text) => text.text.chars().count(),
                            _ => 0,
                        })
                        .sum();
                    format!("{characters} characters")
                });
                return compact(
                    DetailMode::System,
                    "System Prompt",
                    "system-prompt",
                    blocks_are_large(&message.content),
                    rsx! { span { class: "item-text", "{description}" } },
                );
            }
            if matches!(message.message_type, psa::api::events::MessageType::Harness) {
                // Labelled nudges read by their label; unlabelled ones by a preview.
                let summary = match &message.description {
                    Some(description) => rsx! { span { class: "item-text", "{description}" } },
                    None => {
                        let preview = build_line_preview(message.content.iter().filter_map(
                            |block| match block {
                                ContentBlock::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            },
                        ));
                        rsx! { span { class: "item-text item-preview", "{preview}" } }
                    }
                };
                return compact(
                    DetailMode::Harness,
                    "Harness",
                    "harness",
                    blocks_are_large(&message.content),
                    summary,
                );
            }
            if let Some(source) = message
                .source
                .clone()
                .filter(|_| message.message_type == psa::api::events::MessageType::Subagent)
            {
                let sender = source.sender_name.clone();
                let preview =
                    build_line_preview(message.content.iter().filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    }));
                let session_id = kind.context_turn().map(|(target, _)| target.session_id);
                return compact(
                    DetailMode::Received,
                    "Message from",
                    "received",
                    blocks_are_large(&message.content),
                    rsx! {
                        if let Some(session_id) = session_id {
                            AgentLink { session_id, agent_id: source.sender, label: sender, link: MessageLink::Sent { request_id: source.request_id.clone() } }
                        } else {
                            span { class: "item-target", "{sender}" }
                        }
                        span { class: "item-text item-preview", "{preview}" }
                    },
                );
            }
            let is_agent = matches!(message.message_type, psa::api::events::MessageType::Agent);
            let is_human = matches!(message.message_type, psa::api::events::MessageType::Human);
            rsx! { article { class: if is_human { "message human-message" } else if is_agent { "message agent-message" } else { "message" },
                if !is_human && !is_agent { div { class: "role", "{message.message_type:?}" } }
                if provisional { span { class: "streaming-indicator", "Streaming" } }
                if is_agent {
                    for block in &message.content {
                        match block {
                            ContentBlock::Text(text) => rsx! { crate::markdown::Markdown { text: text.text.clone() } },
                            _ => rsx! { pre { "{blocks(std::slice::from_ref(block))}" } },
                        }
                    }
                } else { pre { "{blocks(&message.content)}" } }
            } }
        }
        EventType::Log(log) => {
            // One cut-off line, expanding to the full message like reasoning.
            let (label, class) = if log.error {
                ("ERROR", "log error")
            } else {
                ("INFO", "log")
            };
            let preview = build_line_preview(std::iter::once(log.message.as_str()));
            compact(
                DetailMode::Log,
                label,
                class,
                log.message.len() > LAZY_DETAIL_BYTES,
                rsx! { span { class: "item-text", "{preview}" } },
            )
        }
        EventType::Command(command) => {
            // Right-aligned like the user's messages, but drawn as a control.
            let status = match command.outcome {
                psa::api::CommandOutcome::Succeeded => None,
                psa::api::CommandOutcome::Failed => Some("failed"),
                psa::api::CommandOutcome::Interrupted => Some("interrupted"),
            };
            let class = if status.is_some() {
                "item command failed"
            } else {
                "item command"
            };
            let head = rsx! {
                span { class: "command-name", "/{command.name}" }
                if !command.args.is_empty() { span { class: "command-args", "{command.args}" } }
                if let Some(status) = status { span { class: "item-status failed", "{status}" } }
            };
            if command.output.is_none() {
                return rsx! { div { class, div { class: "command-line", {head} } } };
            }
            rsx! {
                EventDetails { kind: kind.clone(), result: None, mode: DetailMode::Command, class: class.to_owned(), lazy: false, {head} }
            }
        }
        EventType::DecisionCall(call) => compact(
            DetailMode::Decision,
            "Decision",
            if call.response.is_err() {
                "event error"
            } else {
                "event"
            },
            false,
            rsx! { span { class: "item-text", "{client_shared::summarize_decision_call(call)}" } },
        ),
        EventType::ToolCall(call) => {
            let result = result.as_ref().and_then(|result| match result.kind() {
                EventType::ToolResult(result) => Some(result),
                _ => None,
            });
            let failed = result.is_some_and(|result| result.error);
            let summary = summarize_call(&call.tool_name, &call.arguments);
            // Message tools link to the delivery in the recipient's transcript.
            let recipient = is_message_tool(&call.tool_name)
                .then(|| {
                    let sent = result?.delivery.clone()?;
                    let session_id = kind.context_turn()?.0.session_id;
                    let label = serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .ok()
                        .and_then(|arguments| {
                            arguments
                                .get("agent_name")
                                .or_else(|| arguments.get("name"))?
                                .as_str()
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| format!("agent {}", sent.recipient.0));
                    Some((session_id, sent, label))
                })
                .flatten();
            let code = summary
                .code
                .as_ref()
                .map(|(code, language)| (build_code_preview(code), code_language(language)));
            let has_code = code.is_some();

            compact(
                DetailMode::Tool,
                &call.tool_name,
                if failed { "tool error" } else { "tool" },
                call.arguments.len() > LAZY_DETAIL_BYTES
                    || result.is_some_and(|result| blocks_are_large(&result.content)),
                rsx! {
                    if let Some((preview, language)) = code {
                        div { class: "item-code",
                            span { class: "item-prompt", "aria-hidden": "true",
                                if language == crate::syntax_highlight::CodeLanguage::Bash { "$" } else { ">>>" }
                            }
                            div { class: "item-source",
                                crate::syntax_highlight::HighlightedCode { code: preview.text, language }
                                if preview.hidden_lines > 0 {
                                    span { class: "item-more", "+{preview.hidden_lines} more lines" }
                                }
                            }
                        }
                    } else {
                        span { class: "item-text",
                            if let Some((session_id, sent, label)) = recipient {
                                "→ "
                                AgentLink { session_id, agent_id: sent.recipient, label, link: MessageLink::Received { request_id: sent.request_id } }
                                " "
                            }
                            if let Some(target) = &summary.target { span { class: "item-target", "{target}" } " " }
                            if summary.target.is_none() && summary.kwargs.is_empty() { "(no arguments)" }
                            "{summary.kwargs}"
                        }
                    }
                    if has_code && !summary.kwargs.is_empty() { span { class: "item-meta", "{summary.kwargs}" } }
                    if result.is_none() {
                        span { class: "item-status running", role: "img", "aria-label": "Running" }
                    } else if failed {
                        span { class: "item-status failed", "failed" }
                    }
                },
            )
        }
        EventType::ToolResult(result) => compact(
            DetailMode::Unmatched,
            "Tool result",
            "event",
            blocks_are_large(&result.content),
            rsx! { span { class: "item-text", "unmatched · {result.call_id}" } },
        ),
        EventType::Compaction(_) => compact(
            DetailMode::Compaction,
            "Context compaction",
            "event",
            true,
            rsx! {},
        ),
        EventType::ProviderItem(_) => compact(
            DetailMode::Provider,
            "Provider-native event",
            "event",
            true,
            rsx! {},
        ),
        EventType::Thinking(thinking) => {
            if thinking.contents.summary.iter().all(String::is_empty) {
                return rsx! {};
            }
            match preferences.reasoning_display {
                ReasoningDisplay::Hidden => rsx! {},
                ReasoningDisplay::Collapsed => {
                    let preview =
                        build_line_preview(thinking.contents.summary.iter().map(String::as_str));
                    compact(
                        DetailMode::Thinking,
                        "Reasoning",
                        "reasoning",
                        text_parts_are_large(&thinking.contents.summary),
                        rsx! { span { class: "item-text item-preview", "{preview}" } },
                    )
                }
                ReasoningDisplay::Shown => {
                    rsx! { article { class: "message reasoning", pre { "{thinking.contents.summary.join(\"\n\")}" } } }
                }
            }
        }
        EventType::Result(result) => {
            if preferences.hide_text_recorded_results && matches!(result, AgentResult::Text(_)) {
                return rsx! {};
            }
            let (lazy, preview) = match result {
                AgentResult::Text(text) => (
                    text.len() > LAZY_DETAIL_BYTES,
                    build_line_preview([text.as_str()]),
                ),
                AgentResult::Json(value) => {
                    let mut budget = LAZY_DETAIL_BYTES;
                    (json_is_large(value, &mut budget), "JSON".to_owned())
                }
            };
            compact(
                DetailMode::Result,
                "Recorded result",
                "event",
                lazy,
                rsx! { span { class: "item-text item-preview", "{preview}" } },
            )
        }
        EventType::InferenceResultEnd(end) => match &end.outcome {
            llm_api::api::Outcome::Failed(error) => {
                let message = error.to_string();
                let preview = build_line_preview([message.as_str()]);
                rsx! {
                    div { role: "alert",
                        {compact(
                            DetailMode::Error,
                            "FAILED",
                            "error",
                            error_details_are_large(error),
                            rsx! { span { class: "item-text", "Inference failed: {preview}" } },
                        )}
                    }
                }
            }
            llm_api::api::Outcome::Cancelled => line("CANCELLED", "", "Inference cancelled".into()),
            llm_api::api::Outcome::Incomplete { reason } => {
                use llm_api::api::StopReason;
                let reason = match reason {
                    StopReason::EndTurn => "End of turn",
                    StopReason::ToolCalls => "Tool calls",
                    StopReason::StopSequence => "Stop sequence",
                    StopReason::TokenLimit => "Token limit reached",
                    StopReason::ContentFilter => "Content filtered",
                    StopReason::Other(reason) => reason,
                };
                line(
                    "INCOMPLETE",
                    "warning",
                    format!("Inference incomplete: {reason}"),
                )
            }
            llm_api::api::Outcome::Completed { .. } | llm_api::api::Outcome::InProgress => {
                rsx! {}
            }
        },
        EventType::InferenceResultStart(_) => match preferences.turn_dividers {
            TurnDividerDisplay::Hidden => rsx! {},
            TurnDividerDisplay::Line => {
                rsx! { div { class: "turn-divider", role: "separator", span { class: "turn-rule" } } }
            }
            TurnDividerDisplay::Labeled => rsx! { TurnDivider { end: result.clone(), turn } },
        },
        EventType::StatusChange(status) => {
            let interrupted = status.new_status == AgentState::Interrupted;
            if !interrupted && preferences.hide_agent_statuses {
                return rsx! {};
            }
            let state = match status.new_status {
                AgentState::Idle => "Idle",
                AgentState::Running => "Running",
                AgentState::Interrupted => "Interrupted",
            };
            let text = match &status.reason {
                Some(reason) => format!("{state} · {reason}"),
                None => state.to_owned(),
            };
            line("AGENT", if interrupted { "error" } else { "" }, text)
        }
        _ => rsx! {},
    }
}

#[cfg(test)]
use crate::views::Layout;
#[cfg(test)]
#[path = "tests/transcript_view/appearance_event_tests.rs"]
mod appearance_event_tests;
#[cfg(test)]
#[path = "tests/transcript_view/follow_tests.rs"]
mod follow_tests;
#[cfg(test)]
#[path = "tests/transcript_view/inference_result_tests.rs"]
mod inference_result_tests;
#[cfg(test)]
#[path = "tests/transcript_view/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "tests/transcript_view/transcript_visual_tests.rs"]
mod transcript_visual_tests;
