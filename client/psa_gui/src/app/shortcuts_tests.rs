use super::*;
use dioxus::core::{ElementId, Mutation};
use dioxus::html::{Location, PlatformEventData, SerializedKeyboardData};
use std::any::Any;

#[derive(Clone)]
struct Fixture {
    modal: bool,
    settings: bool,
    ui: Rc<RefCell<Option<Signal<Ui>>>>,
}

#[component]
fn RegisteredModal(open: Signal<bool>) -> Element {
    register_modal(open);
    rsx! { div { role: "dialog", "Modal" } }
}

fn shortcut_root() -> Element {
    let fixture = use_context::<Fixture>();
    let mut ui = use_signal(|| {
        let mut ui = Ui::new(DesktopSettings::default());
        let first = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        };
        let second = TranscriptTarget {
            session_id: SessionId(2),
            agent_id: AgentId(1),
        };
        ui.workspace.open(first, true);
        ui.workspace.open(second, true);
        let group = ui
            .workspace
            .split(ui.workspace.focused_group, SplitAxis::Horizontal)
            .unwrap();
        ui.workspace.focused_group = group;
        ui.workspace.open(
            TranscriptTarget {
                session_id: SessionId(3),
                agent_id: AgentId(1),
            },
            true,
        );
        ui.settings_open = fixture.settings;
        ui
    });
    *fixture.ui.borrow_mut() = Some(ui);
    let modals = use_context_provider(ModalRegistry::default);
    let modal = use_signal(|| fixture.modal);
    let state = ui.read();
    let label = format!(
        "{:?} {:?}",
        state.workspace.focused_group,
        state.workspace.active_tab().map(|t| t.target.session_id)
    );
    rsx! {
        div { onkeydown: move |event| handle_workspace_key(ui, &modals, event),
            textarea { onkeydown: move |_| {} }
            if modal() { RegisteredModal { open: modal } }
            span { "{label}" }
            button { onclick: move |_| ui.write().settings_open = false, "Close settings" }
        }
    }
}

fn setup(modal: bool, settings: bool) -> (VirtualDom, Fixture, ElementId) {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let fixture = Fixture {
        modal,
        settings,
        ui: Rc::default(),
    };
    let mut dom = VirtualDom::new(shortcut_root).with_root_context(fixture.clone());
    let edits = dom.rebuild_to_vec();
    let textarea = edits
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .last()
        .unwrap();
    (dom, fixture, textarea)
}

fn press(
    dom: &VirtualDom,
    target: ElementId,
    key: Key,
    code: Code,
    modifiers: Modifiers,
) -> Event<dyn Any> {
    let data = SerializedKeyboardData::new(key, code, Location::Standard, false, modifiers, false);
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("keydown", event.clone(), target);
    event
}

#[test]
fn shortcuts_work_from_textarea_and_target_focused_pane() {
    let (dom, fixture, textarea) = setup(false, false);
    let mut ui = fixture.ui.borrow().unwrap();
    press(
        &dom,
        textarea,
        Key::ArrowLeft,
        Code::ArrowLeft,
        Modifiers::ALT,
    );
    press(
        &dom,
        textarea,
        Key::Character("1".into()),
        Code::Digit1,
        Modifiers::ALT,
    );
    assert_eq!(
        ui.peek().workspace.active_tab().unwrap().target.session_id,
        SessionId(1)
    );
    press(
        &dom,
        textarea,
        Key::Character("2".into()),
        Code::Digit2,
        Modifiers::ALT,
    );
    assert_eq!(
        ui.peek().workspace.active_tab().unwrap().target.session_id,
        SessionId(2)
    );
    press(
        &dom,
        textarea,
        Key::ArrowRight,
        Code::ArrowRight,
        Modifiers::ALT,
    );
    let event = press(
        &dom,
        textarea,
        Key::Character("w".into()),
        Code::KeyW,
        Modifiers::CONTROL,
    );
    assert!(!event.default_action_enabled());
    assert_eq!(
        ui.peek().workspace.active_tab().unwrap().target.session_id,
        SessionId(2)
    );
    assert_eq!(
        ui.peek()
            .workspace
            .groups()
            .iter()
            .map(|g| g.tabs.len())
            .sum::<usize>(),
        2
    );
    let event = press(
        &dom,
        textarea,
        Key::Character("w".into()),
        Code::KeyW,
        Modifiers::CONTROL | Modifiers::SHIFT,
    );
    assert!(event.default_action_enabled());
    ui.write().workspace = Workspace::default();
    // Closing an empty pane is still prevented and is a no-op.
    let event = press(
        &dom,
        textarea,
        Key::Character("w".into()),
        Code::KeyW,
        Modifiers::CONTROL,
    );
    assert!(!event.default_action_enabled());
    assert!(ui.peek().workspace.active_tab().is_none());
}

#[test]
fn follow_shortcut_requires_exact_modifiers() {
    let (dom, fixture, textarea) = setup(false, false);
    let ui = fixture.ui.borrow().unwrap();
    for modifiers in [
        Modifiers::empty(),
        Modifiers::SHIFT,
        Modifiers::CONTROL | Modifiers::SHIFT,
        Modifiers::CONTROL | Modifiers::ALT,
        Modifiers::CONTROL | Modifiers::META,
    ] {
        let event = press(&dom, textarea, Key::End, Code::End, modifiers);
        assert!(event.default_action_enabled());
        assert_eq!(ui.peek().follow_request, (0, None));
    }
    let old = press(&dom, textarea, Key::Escape, Code::Escape, Modifiers::SHIFT);
    assert!(old.default_action_enabled());
    let active = ui.peek().workspace.active_tab().unwrap().id;
    let event = press(&dom, textarea, Key::End, Code::End, Modifiers::CONTROL);
    assert!(!event.default_action_enabled());
    assert_eq!(ui.peek().follow_request, (1, Some(active)));
}

#[test]
fn open_dialogs_block_shortcuts_even_when_focus_is_outside() {
    for (modal, settings) in [(true, false), (false, true)] {
        let (dom, fixture, textarea) = setup(modal, settings);
        let ui = fixture.ui.borrow().unwrap();
        let before = ui.peek().workspace.focused_group;
        for (key, code, modifiers) in [
            (Key::Character("w".into()), Code::KeyW, Modifiers::CONTROL),
            (Key::ArrowLeft, Code::ArrowLeft, Modifiers::ALT),
            (Key::Character("1".into()), Code::Digit1, Modifiers::ALT),
            (Key::End, Code::End, Modifiers::CONTROL),
        ] {
            let event = press(&dom, textarea, key, code, modifiers);
            assert!(!event.default_action_enabled());
        }
        assert_eq!(ui.peek().follow_request, (0, None));
        assert_eq!(ui.peek().workspace.focused_group, before);
        assert_eq!(
            ui.peek().workspace.active_tab().unwrap().target.session_id,
            SessionId(3)
        );
    }
}
