//! View-local bounded transcript rendering. The reducer owns history; each pane
//! owns only a fixed-size window, follow state, and its browser scroll anchor.
use crate::{
    app::Ui,
    composer::{Composer, DraftComposer, Question},
    network,
    reactive::{Shared, use_ui_selector},
};
use client_shared::*;
use dioxus::prelude::*;
use llm_api::api::ContentBlock;
use psa_api::*;
#[cfg(test)]
use std::collections::HashMap;
use std::{cell::RefCell, collections::HashSet, rc::Rc};

const WINDOW_ROWS: usize = 150;
const WINDOW_STEP: usize = WINDOW_ROWS / 2;
const LAZY_DETAIL_BYTES: usize = 4096;

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
            if let psa_api::events::EventType::ToolCall(call) = payload.kind() {
                Some((payload.agent_id(), call.call_id.clone()))
            } else {
                None
            }
        })
        .collect();
    let rows: Vec<_> = current.rows.iter().enumerate().filter(|(_, payload)| {
        !matches!(payload.kind(), psa_api::events::EventType::ToolResult(result) if call_ids.contains(&(payload.agent_id(), result.call_id.clone())))
            && row_is_visible(payload.kind(), &preferences)
    }).map(|(offset, payload)| {
        let result = if let psa_api::events::EventType::ToolCall(call) = payload.kind() {
            transcript.and_then(|t| t.tool_result(payload.agent_id(), &call.call_id))
        } else { None };
        (current.start + offset, row_key(&payload.id()), payload.clone(), result)
    }).collect();
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
    rsx! {
        div {
            id: dom_id, class: "transcript", "data-window-start": start, "data-window-end": end,
            "data-row-count": count, "data-first-row": first, "data-active": active,
            "data-following": following(), "data-follow-revision": follow_revision(),
            "data-scroll-revision": scroll_revision(), "data-has-older": has_older, "data-history-busy": history_busy(),
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
            for (index, key, payload, result) in rows {
                div { key: "{key}", "data-transcript-row": key, "data-row-index": index,
                    CachedEventRow { payload, result, preferences: RowPreferences::from(&preferences) }
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
        if target.agent_id == crate::app::DRAFT_ROOT { DraftComposer { tab } } else { Composer { tab } }
    }
}

fn row_is_visible(kind: &psa_api::events::EventType, preferences: &UiPreferences) -> bool {
    use psa_api::events::EventType;
    match kind {
        EventType::Message(_)
        | EventType::Log(_)
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
        EventType::InferenceResultEnd(end) => !matches!(
            end.outcome,
            llm_api::api::Outcome::Completed { .. } | llm_api::api::Outcome::InProgress
        ),
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
}
impl From<&UiPreferences> for RowPreferences {
    fn from(preferences: &UiPreferences) -> Self {
        Self {
            reasoning_display: preferences.reasoning_display,
            hide_text_recorded_results: preferences.hide_text_recorded_results,
            hide_agent_statuses: preferences.hide_agent_statuses,
        }
    }
}

#[component]
fn CachedEventRow(
    payload: RowPayload,
    result: Option<RowPayload>,
    preferences: RowPreferences,
) -> Element {
    #[cfg(test)]
    if let Some(counters) = try_use_context::<tests::RenderCounters>() {
        counters.record_row(payload.id());
    }
    rsx! { EventContent { kind: EventKind::Payload(payload), result: result.map(EventKind::Payload), preferences } }
}

#[derive(Clone)]
enum EventKind {
    Payload(RowPayload),
    #[cfg(test)]
    Owned(Shared<psa_api::events::EventType>),
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
    fn kind(&self) -> &psa_api::events::EventType {
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
}

// Compatibility for standalone row renderers/tests. Actual transcript rows use
// CachedEventRow above, with no context access and no global Ui subscription.
#[cfg(test)]
#[derive(Props, Clone)]
pub(crate) struct EventRowProps {
    kind: psa_api::events::EventType,
    provisional: bool,
    results: Rc<HashMap<String, psa_api::events::ToolResult>>,
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
    let result = if let psa_api::events::EventType::ToolCall(call) = &props.kind {
        props
            .results
            .get(&call.call_id)
            .cloned()
            .map(psa_api::events::EventType::ToolResult)
            .map(Shared::new)
            .map(EventKind::Owned)
    } else {
        None
    };
    rsx! { EventContent { kind: EventKind::Owned(Shared::new(props.kind)), result, preferences: RowPreferences::from(&preferences()), provisional: props.provisional } }
}

fn blocks(content: &[ContentBlock]) -> String {
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
                content.content_type
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
    Tool,
    Unmatched,
    Compaction,
    Provider,
    Thinking,
    Result,
    Error,
}

#[component]
fn EventDetails(
    kind: EventKind,
    result: Option<EventKind>,
    mode: DetailMode,
    title: String,
    class: String,
    lazy: bool,
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
            }, "{title}" }
            if expanded() || !lazy {
                DetailBody { kind, result, mode }
            }
        }
    }
}

#[component]
fn DetailBody(kind: EventKind, result: Option<EventKind>, mode: DetailMode) -> Element {
    use psa_api::events::EventType;
    match (mode, kind.kind()) {
        (DetailMode::System, EventType::Message(message)) => {
            rsx! { pre { "{blocks(&message.content)}" } }
        }
        (DetailMode::Tool, EventType::ToolCall(call)) => rsx! {
            h4 { "Arguments" } pre { "{call.arguments}" }
            if let Some(EventType::ToolResult(result)) = result.as_ref().map(EventKind::kind) { h4 { "Result" } pre { "{blocks(&result.content)}" } }
        },
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
            llm_api::api::Outcome::Failed(error) => {
                rsx! { pre { "{serde_json::to_string_pretty(error).unwrap_or_default()}" } }
            }
            _ => rsx! {},
        },
        _ => rsx! {},
    }
}

#[component]
fn EventContent(
    kind: EventKind,
    result: Option<EventKind>,
    preferences: RowPreferences,
    #[props(default)] provisional: bool,
) -> Element {
    use psa_api::events::EventType;
    let provisional = provisional || kind.is_provisional();
    let details = |mode, title: String, class: &str, lazy| rsx! { EventDetails { kind: kind.clone(), result: result.clone(), mode, title, class: class.to_owned(), lazy } };
    match kind.kind() {
        EventType::Message(message) => {
            if matches!(message.message_type, psa_api::events::MessageType::System) {
                let title = message
                    .description
                    .as_ref()
                    .map(|description| format!("System Prompt · {description}"))
                    .unwrap_or_else(|| "System Prompt".into());
                return details(
                    DetailMode::System,
                    title,
                    "tool system-prompt",
                    blocks_are_large(&message.content),
                );
            }
            let is_agent = matches!(
                message.message_type,
                psa_api::events::MessageType::Agent | psa_api::events::MessageType::Subagent
            );
            let is_human = matches!(message.message_type, psa_api::events::MessageType::Human);
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
            rsx! { article { class: if log.error { "message error" } else { "message muted" }, div { class: "role", "Harness log" } pre { "{log.message}" } } }
        }
        EventType::ToolCall(call) => {
            let result = result.as_ref().and_then(|result| match result.kind() {
                EventType::ToolResult(result) => Some(result),
                _ => None,
            });
            let status = result
                .map(|result| if result.error { "error" } else { "complete" })
                .unwrap_or("running / awaiting result");
            let preview: String = call.arguments.chars().take(100).collect();
            details(
                DetailMode::Tool,
                format!("{} · {status} · {preview}", call.tool_name),
                if result.is_some_and(|result| result.error) {
                    "tool error"
                } else {
                    "tool"
                },
                call.arguments.len() > LAZY_DETAIL_BYTES
                    || result.is_some_and(|result| blocks_are_large(&result.content)),
            )
        }
        EventType::ToolResult(result) => details(
            DetailMode::Unmatched,
            format!("Unmatched tool result · {}", result.call_id),
            "tool",
            blocks_are_large(&result.content),
        ),
        EventType::Compaction(_) => details(
            DetailMode::Compaction,
            "Context compaction".into(),
            "tool",
            true,
        ),
        EventType::ProviderItem(_) => details(
            DetailMode::Provider,
            "Provider-native event".into(),
            "tool",
            true,
        ),
        EventType::Thinking(thinking) => {
            if thinking.contents.summary.iter().all(String::is_empty) {
                return rsx! {};
            }
            match preferences.reasoning_display {
                ReasoningDisplay::Hidden => rsx! {},
                ReasoningDisplay::Collapsed => details(
                    DetailMode::Thinking,
                    "Reasoning".into(),
                    "tool reasoning",
                    text_parts_are_large(&thinking.contents.summary),
                ),
                ReasoningDisplay::Shown => {
                    rsx! { article { class: "message reasoning", pre { "{thinking.contents.summary.join(\"\n\")}" } } }
                }
            }
        }
        EventType::Result(result) => {
            if preferences.hide_text_recorded_results && matches!(result, AgentResult::Text(_)) {
                return rsx! {};
            }
            let lazy = match result {
                AgentResult::Text(text) => text.len() > LAZY_DETAIL_BYTES,
                AgentResult::Json(value) => {
                    let mut budget = LAZY_DETAIL_BYTES;
                    json_is_large(value, &mut budget)
                }
            };
            details(DetailMode::Result, "Recorded result".into(), "tool", lazy)
        }
        EventType::InferenceResultEnd(end) => match &end.outcome {
            llm_api::api::Outcome::Failed(error) => {
                rsx! { article { class: "message error", role: "alert", div { class: "role", "Inference failed" } pre { "{error}" } EventDetails { kind: kind.clone(), result: None, mode: DetailMode::Error, title: "Error details".to_owned(), class: "".to_owned(), lazy: error_details_are_large(error) } } }
            }
            llm_api::api::Outcome::Cancelled => {
                rsx! { article { class: "message muted", div { class: "role", "Inference cancelled" } } }
            }
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
                rsx! { article { class: "message", div { class: "role", "Inference incomplete" } pre { "{reason}" } } }
            }
            llm_api::api::Outcome::Completed { .. } | llm_api::api::Outcome::InProgress => rsx! {},
        },
        EventType::StatusChange(status) => {
            if status.new_status == AgentState::Interrupted {
                rsx! { div { class: "error", "Agent Interrupted" } }
            } else if preferences.hide_agent_statuses {
                rsx! {}
            } else {
                rsx! { div { class: "muted", "Agent {status.new_status:?}" } }
            }
        }
        _ => rsx! {},
    }
}

#[cfg(test)]
use crate::views::Layout;
#[cfg(test)]
#[path = "appearance_event_tests.rs"]
mod appearance_event_tests;
#[cfg(test)]
#[path = "follow_tests.rs"]
mod follow_tests;
#[cfg(test)]
#[path = "inference_result_tests.rs"]
mod inference_result_tests;
#[cfg(test)]
#[path = "transcript_view_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "transcript_visual_tests.rs"]
mod transcript_visual_tests;
