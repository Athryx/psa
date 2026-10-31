use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{PlatformEventData, SerializedMouseData},
};
use std::{any::Any, cell::RefCell};

#[derive(Clone, Default)]
pub(super) struct RenderCounters(Rc<RefCell<(usize, HashMap<RowId, usize>)>>);
impl PartialEq for RenderCounters {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl RenderCounters {
    pub(super) fn record_view(&self) {
        self.0.borrow_mut().0 += 1;
    }
    pub(super) fn record_row(&self, id: RowId) {
        *self.0.borrow_mut().1.entry(id).or_default() += 1;
    }
    fn snapshot(&self) -> (usize, HashMap<RowId, usize>) {
        self.0.borrow().clone()
    }
}

fn pending(id: usize) -> PendingEvent {
    PendingEvent {
        id: format!("{id:06}"),
        revision: 1,
        session_id: SessionId(1),
        agent_id: AgentId(1),
        kind: psa_api::events::EventType::Message(psa_api::events::Message::text(
            psa_api::events::MessageType::Human,
            format!("row-{id}"),
        )),
    }
}

#[test]
fn viewport_messages_keep_window_requests_and_coarse_scroll_commits_distinct() {
    let request = serde_json::from_value::<ViewportRequest>(serde_json::json!({
        "kind": "follow", "revision": 3,
        "metrics": { "height": 60000.0, "top": 59400.0, "client_height": 600.0 },
    }))
    .unwrap();
    let ViewportRequest::Follow { revision, metrics } = request else {
        panic!("expected an explicit follow request");
    };
    assert_eq!(revision, 3);
    assert_eq!(metrics.as_tuple(), (60000.0, 59400.0, 600.0));

    let request = serde_json::from_value::<ViewportRequest>(serde_json::json!({
        "kind": "window", "start": 75, "first": "c:100", "count": 1000,
        "history": true, "following": false, "follow_revision": 3,
        "metrics": { "height": 60000.0, "top": 20.0, "client_height": 600.0 },
    }))
    .unwrap();
    let ViewportRequest::Window {
        start,
        history,
        following,
        follow_revision,
        metrics,
        ..
    } = request
    else {
        panic!("expected a window request");
    };
    assert_eq!(start, 75);
    assert!(history);
    assert!(!following);
    assert_eq!(follow_revision, 3);
    assert_eq!(metrics.as_tuple(), (60000.0, 20.0, 600.0));

    let request = serde_json::from_value::<ViewportRequest>(serde_json::json!({
        "kind": "scroll", "following": false, "revision": 4, "follow_revision": 3,
        "metrics": { "height": 60000.0, "top": 59399.0, "client_height": 600.0 },
    }))
    .unwrap();
    let ViewportRequest::Scroll {
        following,
        revision,
        follow_revision,
        metrics,
    } = request
    else {
        panic!("expected a scroll commit");
    };
    assert!(!following);
    assert_eq!((revision, follow_revision), (4, 3));
    assert_eq!(metrics.as_tuple(), (60000.0, 59399.0, 600.0));
}

#[test]
fn follow_window_stays_bounded_during_ten_thousand_appends() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    let window = ViewWindow::default();
    let preferences = UiPreferences::default();
    for id in 0..10_000 {
        transcript.apply(StreamMessage::Upsert {
            pending: pending(id),
        });
        let start = window.start(&transcript, &preferences, true);
        let rows = transcript.display_window(start, WINDOW_ROWS, &preferences);
        assert!(rows.len() <= WINDOW_ROWS);
        assert_eq!(
            rows.last().unwrap().id(),
            RowId::Pending(format!("{id:06}"))
        );
    }
}

#[test]
fn browsing_window_retains_event_identity_when_another_pane_prepends() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    let preferences = UiPreferences::default();
    for id in 1000..1500 {
        transcript.apply(StreamMessage::Upsert {
            pending: pending(id),
        });
    }
    let window = ViewWindow::select(&transcript, &preferences, 100);
    let first = transcript
        .display_id_at(window.start(&transcript, &preferences, false), &preferences)
        .unwrap();
    for id in 0..500 {
        transcript.apply(StreamMessage::Upsert {
            pending: pending(id),
        });
    }
    assert_eq!(
        transcript.display_id_at(window.start(&transcript, &preferences, false), &preferences),
        Some(first)
    );
    assert_eq!(
        transcript
            .display_window(
                window.start(&transcript, &preferences, false),
                WINDOW_ROWS,
                &preferences
            )
            .len(),
        WINDOW_ROWS
    );
}

#[component]
fn RenderFixture(counters: RenderCounters) -> Element {
    let tab = Tab {
        id: ViewId(1),
        target: TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        },
        preview: false,
        last_focused: 0,
    };
    let target = tab.target;
    let mut ui = use_signal(move || {
        let mut ui = Ui::new(DesktopSettings::default());
        let mut transcript = Transcript::new(target.session_id, Some(target.agent_id));
        for id in 0..1000 {
            transcript.apply(StreamMessage::Upsert {
                pending: pending(id),
            });
        }
        ui.transcripts.insert(target, transcript.into());
        ui
    });
    let mut active = use_signal(|| true);
    let mut next = use_signal(|| 1000_usize);
    use_context_provider(|| ui);
    use_context_provider(|| counters);
    rsx! {
        button { onclick: move |_| { let _ = ui.write(); }, "No-op" }
        button { onclick: move |_| { ui.write().drafts.insert(target, "unrelated composer draft".into()); }, "Draft" }
        button { onclick: move |_| {
            let mut event = pending(999); event.revision = 2;
            event.kind = psa_api::events::EventType::Message(psa_api::events::Message::text(psa_api::events::MessageType::Human, "revised-row"));
            ui.write().transcripts.get_mut(&target).unwrap().apply(StreamMessage::Upsert { pending: event });
        }, "Revise" }
        button { onclick: move |_| active.toggle(), "Visibility" }
        button { onclick: move |_| {
            let start = *next.peek();
            let mut state = ui.write();
            let transcript = state.transcripts.get_mut(&target).unwrap();
            for id in start..start + 1000 { transcript.apply(StreamMessage::Upsert { pending: pending(id) }); }
            next.set(start + 1000);
        }, "Append batch" }
        button { onclick: move |_| { ui.write().settings.preferences.transcript_font_size += 1; }, "Font" }
        TranscriptView { tab, active: active() }
    }
}

fn click(dom: &mut VirtualDom, id: ElementId) {
    dom.runtime().handle_event(
        "click",
        Event::new(
            Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
            true,
        ),
        id,
    );
}
async fn settle(dom: &mut VirtualDom) {
    for _ in 0..8 {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(2), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
    }
}
fn buttons(edits: &dioxus::core::Mutations) -> Vec<ElementId> {
    edits
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .take(6)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_rows_ignore_unrelated_writes_and_only_changed_payload_rerenders() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let counters = RenderCounters::default();
    let mut dom = VirtualDom::new_with_props(
        RenderFixture,
        RenderFixtureProps {
            counters: counters.clone(),
        },
    );
    let edits = dom.rebuild_to_vec();
    let actions = buttons(&edits);
    settle(&mut dom).await;
    let before = counters.snapshot();
    assert_eq!(before.1.len(), WINDOW_ROWS);
    for id in &actions[..2] {
        click(&mut dom, *id);
        settle(&mut dom).await;
    }
    assert_eq!(
        counters.snapshot(),
        before,
        "draft/no-op writes must not render transcript or rows"
    );
    click(&mut dom, actions[5]);
    settle(&mut dom).await;
    assert_eq!(
        counters.snapshot().1,
        before.1,
        "font/width styling must not invalidate row memoization"
    );
    click(&mut dom, actions[2]);
    settle(&mut dom).await;
    let after = counters.snapshot();
    for (id, count) in &before.1 {
        assert_eq!(
            after.1[id],
            count + usize::from(*id == RowId::Pending("000999".into())),
            "{id:?}"
        );
    }
    assert!(dioxus_ssr::render(&dom).contains("revised-row"));
}

#[tokio::test(flavor = "current_thread")]
async fn hidden_views_suspend_live_rendering_and_resume_with_bounded_rows() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let counters = RenderCounters::default();
    let mut dom = VirtualDom::new_with_props(
        RenderFixture,
        RenderFixtureProps {
            counters: counters.clone(),
        },
    );
    let edits = dom.rebuild_to_vec();
    let actions = buttons(&edits);
    settle(&mut dom).await;
    click(&mut dom, actions[3]);
    settle(&mut dom).await;
    let hidden = counters.snapshot();
    click(&mut dom, actions[4]);
    settle(&mut dom).await;
    assert_eq!(
        counters.snapshot(),
        hidden,
        "a hidden mounted pane must not rebuild on live data"
    );
    click(&mut dom, actions[3]);
    settle(&mut dom).await;
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("row-1999"));
    assert_eq!(html.matches("data-transcript-row=").count(), WINDOW_ROWS);
}

#[tokio::test(flavor = "current_thread")]
async fn mounted_dom_never_grows_after_ten_thousand_additional_events() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new_with_props(
        RenderFixture,
        RenderFixtureProps {
            counters: RenderCounters::default(),
        },
    );
    let edits = dom.rebuild_to_vec();
    let actions = buttons(&edits);
    settle(&mut dom).await;
    for _ in 0..10 {
        click(&mut dom, actions[4]);
        settle(&mut dom).await;
        assert_eq!(
            dioxus_ssr::render(&dom)
                .matches("data-transcript-row=")
                .count(),
            WINDOW_ROWS
        );
    }
    assert!(dioxus_ssr::render(&dom).contains("row-10999"));
}

#[test]
fn oversized_collapsed_system_prompt_does_not_build_its_body() {
    let content = format!("large-system-secret{}", "x".repeat(LAZY_DETAIL_BYTES));
    let kind = psa_api::events::EventType::Message(psa_api::events::Message::text(
        psa_api::events::MessageType::System,
        content,
    ));
    let html = dioxus_ssr::render_element(
        rsx! { EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) } },
    );
    assert!(html.contains("System Prompt"));
    assert!(!html.contains("large-system-secret"));
}

#[component]
fn ExpansionPane() -> Element {
    use_context_provider(ExpansionState::default);
    let mut mounted = use_signal(|| true);
    let payload = use_hook(|| {
        let mut event = pending(1);
        event.kind = psa_api::events::EventType::Message(psa_api::events::Message::text(
            psa_api::events::MessageType::System,
            format!("expanded-secret{}", "x".repeat(LAZY_DETAIL_BYTES)),
        ));
        RowPayload::Pending(std::sync::Arc::new(event))
    });
    rsx! {
        button { onclick: move |_| mounted.toggle(), "Recycle" }
        if mounted() { CachedEventRow { payload, result: None, preferences: RowPreferences::from(&UiPreferences::default()) } }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn recycled_details_remember_expansion_independently_in_duplicate_panes() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    fn root() -> Element {
        rsx! { div { ExpansionPane {} ExpansionPane {} } }
    }
    let mut dom = VirtualDom::new(root);
    let edits = dom.rebuild_to_vec();
    let actions = buttons(&edits);
    settle(&mut dom).await;
    assert!(!dioxus_ssr::render(&dom).contains("expanded-secret"));
    click(&mut dom, actions[1]);
    settle(&mut dom).await;
    assert_eq!(
        dioxus_ssr::render(&dom).matches("expanded-secret").count(),
        1
    );
    click(&mut dom, actions[0]);
    settle(&mut dom).await;
    assert!(!dioxus_ssr::render(&dom).contains("expanded-secret"));
    click(&mut dom, actions[0]);
    settle(&mut dom).await;
    assert_eq!(
        dioxus_ssr::render(&dom).matches("expanded-secret").count(),
        1,
        "only the first pane restores its expansion on remount"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn large_details_materialize_on_expansion_and_unmount_on_collapse() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    fn root() -> Element {
        let kind = psa_api::events::EventType::Result(AgentResult::Json(
            serde_json::json!({ "large-secret": "x".repeat(LAZY_DETAIL_BYTES * 4) }),
        ));
        rsx! { EventContent { kind: EventKind::Owned(Shared::new(kind)), result: None, preferences: RowPreferences::from(&UiPreferences::default()) } }
    }
    let mut dom = VirtualDom::new(root);
    let edits = dom.rebuild_to_vec();
    let summary = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(!dioxus_ssr::render(&dom).contains("large-secret"));
    click(&mut dom, summary);
    settle(&mut dom).await;
    assert!(dioxus_ssr::render(&dom).contains("large-secret"));
    click(&mut dom, summary);
    settle(&mut dom).await;
    assert!(!dioxus_ssr::render(&dom).contains("large-secret"));
}
