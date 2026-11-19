use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{
        Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData,
        SerializedMouseData, SerializedScrollData,
    },
};
use std::{any::Any, cell::Cell, rc::Rc};

#[test]
fn shared_history_prepend_does_not_expand_another_panes_visible_tail() {
    assert_eq!(updated_history_limit(20, None, 20, Some(20), 0), 20);
    assert_eq!(
        updated_history_limit(20, Some((20, Some(20))), 170, Some(10), 0),
        20
    );
    assert_eq!(
        updated_history_limit(20, Some((20, Some(20))), 21, Some(20), 1),
        21
    );
    assert_eq!(
        updated_history_limit(20, Some((20, Some(20))), 19, Some(20), 0),
        19
    );
    assert_eq!(
        updated_history_limit(20, Some((20, Some(20))), 171, Some(10), 1),
        21
    );
}

#[test]
fn history_continues_for_invisible_or_short_pages() {
    assert!(history_needs_continuation(500.0, 500.0, 300.0));
    assert!(history_needs_continuation(100.0, 200.0, 300.0));
    assert!(!history_needs_continuation(500.0, 700.0, 300.0));
}

#[test]
fn history_anchor_preserves_the_distance_from_existing_content() {
    assert_eq!(restored_history_top(1000.0, 24.0, 1400.0), 424.0);
    assert_eq!(restored_history_top(1000.0, 24.0, 900.0), 0.0);
}

#[test]
fn history_trigger_requires_a_new_top_entry_after_failure_and_is_per_pane() {
    let mut first = false;
    let mut second = false;
    assert!(!history_trigger(0.0, false, &mut first));
    assert!(history_trigger(0.0, true, &mut first));
    assert!(!history_trigger(0.0, true, &mut first));
    assert!(history_trigger(0.0, true, &mut second));
    assert!(!history_trigger(81.0, true, &mut first));
    assert!(history_trigger(80.0, true, &mut first));
    assert!(!history_trigger(0.0, true, &mut second));
}

#[test]
fn history_reveals_cached_rows_before_fetching_and_stops_when_busy_or_exhausted() {
    assert_eq!(history_action(151, 150, true, false), HistoryAction::Reveal);
    assert_eq!(history_action(151, 150, true, true), HistoryAction::Reveal);
    assert_eq!(history_action(150, 150, true, false), HistoryAction::Fetch);
    assert_eq!(history_action(150, 150, true, true), HistoryAction::None);
    assert_eq!(history_action(150, 150, false, false), HistoryAction::None);
}

#[component]
fn HistoryFixture() -> Element {
    let tab = fixture_pane(1).tabs.remove(0);
    let target = tab.target;
    let ui = use_signal(move || {
        let mut ui = Ui::new(DesktopSettings::default());
        let mut transcript = Transcript::new(target.session_id, Some(target.agent_id));
        for i in 0..151 {
            let pending = PendingEvent {
                id: format!("{i:03}").into(),
                revision: 1,
                session_id: target.session_id,
                agent_id: target.agent_id,
                kind: psa::api::events::EventType::Message(psa::api::events::Message::text(
                    psa::api::events::MessageType::Human,
                    format!("history-item-{i:03}"),
                )),
            };
            transcript.apply(StreamMessage::Upsert { pending });
        }
        ui.transcripts.insert(target, transcript.into());
        ui
    });
    use_context_provider(|| ui);
    rsx! { TranscriptView { tab, active: true } }
}

#[tokio::test(flavor = "current_thread")]
async fn scrolling_to_top_reveals_cached_history_without_manual_buttons() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(HistoryFixture);
    let edits = dom.rebuild_to_vec();
    let scroll = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "scroll" => Some(*id),
            _ => None,
        })
        .unwrap();
    // The message navigator previews every loaded turn; only rows are windowed.
    let first_row = "<pre>history-item-000</pre>";
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("<pre>history-item-150</pre>"));
    assert!(!html.contains(first_row));
    assert!(!html.contains("Load older history"));
    assert!(!html.contains("earlier loaded items"));
    let data = SerializedScrollData {
        scroll_top: 0.0,
        scroll_left: 0.0,
        scroll_width: 500,
        scroll_height: 5000,
        client_width: 500,
        client_height: 500,
    };
    dom.runtime().handle_event(
        "scroll",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
            true,
        ),
        scroll,
    );
    for _ in 0..5 {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
    }
    assert!(dioxus_ssr::render(&dom).contains(first_row));
}

#[component]
fn EmptyHistoryFixture() -> Element {
    let tab = fixture_pane(1).tabs.remove(0);
    let target = tab.target;
    let mut ui = use_signal(move || {
        let mut ui = Ui::new(DesktopSettings::default());
        ui.transcripts.insert(
            target,
            Transcript::new(target.session_id, Some(target.agent_id)).into(),
        );
        ui
    });
    use_context_provider(|| ui);
    rsx! {
        button { onclick: move |_| {
            ui.write().transcripts.get_mut(&target).unwrap().apply(StreamMessage::Upsert {
                pending: PendingEvent {
                    id: "new".into(), revision: 1, session_id: target.session_id, agent_id: target.agent_id,
                    kind: psa::api::events::EventType::Message(psa::api::events::Message::text(
                        psa::api::events::MessageType::Human, "new-live-row")),
                },
            });
        }, "Add live row" }
        TranscriptView { tab, active: true }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn empty_transcript_displays_later_live_rows() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(EmptyHistoryFixture);
    let edits = dom.rebuild_to_vec();
    let button = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    dom.runtime().handle_event(
        "click",
        Event::new(
            Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
            true,
        ),
        button,
    );
    for _ in 0..5 {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
    }
    assert!(dioxus_ssr::render(&dom).contains("new-live-row"));
}

#[component]
fn FollowFixture(renders: Rc<Cell<usize>>) -> Element {
    let following = use_signal(|| true);
    renders.set(renders.get() + 1);
    let current = following();
    rsx! {
        button { onclick: move |_| update_following_from_scroll(following, true), "near" }
        button { onclick: move |_| update_following_from_scroll(following, false), "away" }
        div { "{current}" }
    }
}

#[test]
fn scroll_within_threshold_does_not_retrigger_follow_subscribers() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let renders = Rc::new(Cell::new(0));
    let mut dom = VirtualDom::new_with_props(
        FollowFixture,
        FollowFixtureProps {
            renders: renders.clone(),
        },
    );
    let edits = dom.rebuild_to_vec();
    let buttons: Vec<ElementId> = edits
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(buttons.len(), 2);
    let initial = renders.get();
    let click = |dom: &mut VirtualDom, id| {
        dom.runtime().handle_event(
            "click",
            Event::new(
                Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default()))
                    as Rc<dyn Any>,
                true,
            ),
            id,
        );
        dom.render_immediate_to_vec();
    };
    click(&mut dom, buttons[0]);
    assert_eq!(
        renders.get(),
        initial,
        "remaining near bottom must not notify subscribers"
    );
    click(&mut dom, buttons[1]);
    assert!(
        renders.get() > initial,
        "leaving the threshold must update follow state"
    );
    let away = renders.get();
    click(&mut dom, buttons[1]);
    assert_eq!(
        renders.get(),
        away,
        "remaining away must not notify subscribers"
    );
    click(&mut dom, buttons[0]);
    assert!(
        renders.get() > away,
        "returning near the bottom must resume follow"
    );
}

fn fixture_pane(id: u64) -> TabGroup {
    TabGroup {
        id: GroupId(id),
        tabs: vec![Tab {
            id: ViewId(id),
            target: TranscriptTarget {
                session_id: SessionId(id),
                agent_id: psa::api::AgentId(id),
            },
            kind: TabKind::Transcript,
            preview: false,
            last_focused: 0,
        }],
        active: Some(ViewId(id)),
    }
}

fn split_fixture() -> Element {
    let mut ui = use_signal(|| {
        let mut ui = Ui::new(DesktopSettings::default());
        ui.settings.preferences.auto_follow = false;
        ui.workspace.root = SplitNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(SplitNode::Group(fixture_pane(1))),
            second: Box::new(SplitNode::Group(fixture_pane(2))),
        };
        ui
    });
    use_context_provider(|| ui);
    let modals = use_context_provider(crate::app::ModalRegistry::default);
    rsx! { div { onkeydown: move |event| crate::app::handle_workspace_key(ui, &modals, event),
        button { onclick: move |_| ui.write().workspace.focused_group = GroupId(2), "Sidebar" }
        Layout { node: ui.read().workspace.root.clone(), path: vec![] }
    } }
}

#[test]
fn ctrl_end_follows_only_the_active_pane() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(split_fixture);
    let edits = dom.rebuild_to_vec();
    let root = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .unwrap();
    let sidebar = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    let textarea = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "input" => Some(*id),
            _ => None,
        })
        .unwrap();
    let input = SerializedFormData::new("/".into(), vec![]);
    dom.runtime().handle_event(
        "input",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(input))) as Rc<dyn Any>,
            true,
        ),
        textarea,
    );
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("Slash commands"));
    assert_eq!(
        dioxus_ssr::render(&dom)
            .matches("Follow new activity")
            .count(),
        2
    );
    let data = SerializedKeyboardData::new(
        Key::End,
        Code::End,
        Location::Standard,
        false,
        Modifiers::CONTROL,
        false,
    );
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime()
        .handle_event("keydown", event.clone(), textarea);
    assert!(!event.default_action_enabled());
    dom.render_immediate_to_vec();
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Slash commands"));
    assert_eq!(html.matches("Follow new activity").count(), 1);

    dom.runtime().handle_event(
        "click",
        Event::new(
            Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
            true,
        ),
        sidebar,
    );
    dom.render_immediate_to_vec();
    let data = SerializedKeyboardData::new(
        Key::End,
        Code::End,
        Location::Standard,
        false,
        Modifiers::CONTROL,
        false,
    );
    dom.runtime().handle_event(
        "keydown",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
            true,
        ),
        root,
    );
    dom.render_immediate_to_vec();
    dom.render_immediate_to_vec();
    assert_eq!(
        dioxus_ssr::render(&dom)
            .matches("Follow new activity")
            .count(),
        0
    );
}

#[derive(Clone)]
struct ScrollRecorder(Rc<Cell<usize>>);

impl dioxus::html::RenderedElementBacking for ScrollRecorder {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn scroll(
        &self,
        coordinates: dioxus::html::geometry::PixelsVector2D,
        behavior: ScrollBehavior,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = dioxus::html::MountedResult<()>>>> {
        assert_eq!(coordinates.x, 0.0);
        assert!(coordinates.y >= 1.0e9);
        assert_eq!(behavior, ScrollBehavior::Instant);
        self.0.set(self.0.get() + 1);
        Box::pin(async { Ok(()) })
    }
}

#[component]
fn ScrollFixture(calls: Rc<Cell<usize>>) -> Element {
    let element = use_signal(|| Some(Rc::new(MountedData::new(ScrollRecorder(calls)))));
    rsx! { button { onclick: move |_| scroll_transcript_to_bottom(element), "follow" } }
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_follow_requests_scroll_the_container_instantly() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let calls = Rc::new(Cell::new(0));
    let mut dom = VirtualDom::new_with_props(
        ScrollFixture,
        ScrollFixtureProps {
            calls: calls.clone(),
        },
    );
    let edits = dom.rebuild_to_vec();
    let button = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    for expected in 1..=2 {
        dom.runtime().handle_event(
            "click",
            Event::new(
                Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default()))
                    as Rc<dyn Any>,
                true,
            ),
            button,
        );
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
        assert_eq!(calls.get(), expected);
    }
}
