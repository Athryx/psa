use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{
        Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData,
        SerializedMouseData,
    },
};
use std::{any::Any, cell::Cell, rc::Rc};

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
                agent_id: psa_api::AgentId(id),
            },
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
