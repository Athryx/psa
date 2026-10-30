use super::*;
use client_shared::*;
use psa_api::{
    AgentId, SessionId,
    events::{EventType, Message, MessageType},
};
use std::{cell::RefCell, collections::HashMap};

#[derive(Default)]
struct FakePlatform {
    saved: RefCell<Vec<DesktopSettings>>,
}
impl GuiPlatform for FakePlatform {
    fn save_settings(
        &self,
        settings: DesktopSettings,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async move {
            self.saved.borrow_mut().push(settings);
            Ok(())
        })
    }
}

#[test]
fn root_renders_without_native_renderer_or_live_server() {
    let platform = Rc::new(FakePlatform::default());
    let bootstrap = GuiBootstrap {
        config: GuiConfig {
            settings: DesktopSettings::default(),
        },
        platform,
    };
    let mut dom = VirtualDom::new(App).with_root_context(bootstrap);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Workspace navigation"));
    assert!(html.contains("Sessions"));
    assert!(html.contains("Settings"));
    assert!(html.contains("Select an agent"));
}

fn folder_create_test_root() -> Element {
    let mut ui = use_signal(|| {
        let mut state = crate::app::Ui::new(DesktopSettings::default());
        state.connection = "Connected".into();
        state
    });
    use_context_provider(|| ui);
    let probe = psa_api::ProbeId("00000000-0000-0000-0000-000000000000".parse().unwrap());
    let target = crate::creation::CreationTarget {
        probe,
        directory: "/work/project".into(),
    };
    rsx! {
        div { onclick: move |_| ui.write().notice = Some("outer click".into()),
            onkeydown: move |_| ui.write().notice = Some("outer key".into()),
            button { onclick: move |event| { event.stop_propagation(); crate::creation::select_target(&mut ui.write(), target.clone()); }, "+" }
            if let Some(target) = ui.read().creation_target.clone() {
                crate::creation::InlineSessionName { target }
            }
            if let Some(notice) = &ui.read().notice { "{notice}" }
        }
    }
}

#[test]
fn folder_create_is_inline_and_does_not_bubble() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(folder_create_test_root);
    let mutations = dom.rebuild_to_vec();
    let button = mutations
        .edits
        .iter()
        .filter_map(|edit| match edit {
            dioxus::core::Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .last()
        .unwrap();
    click(&dom, button.0);
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Session name\""), "{html}");
    assert!(!html.contains("role=\"dialog\""), "{html}");
    assert!(!html.contains("outer click"), "{html}");
}

fn transient_folder_test_root() -> Element {
    let ui = use_signal(|| {
        let mut state = crate::app::Ui::new(DesktopSettings::default());
        state.connection = "Connected".into();
        state.creation_target = Some(crate::creation::CreationTarget {
            probe: psa_api::ProbeId("00000000-0000-0000-0000-000000000000".parse().unwrap()),
            directory: "/remote/new".into(),
        });
        state
    });
    use_context_provider(|| ui);
    rsx! { crate::views::Sidebar {} }
}

#[test]
fn selected_unloaded_directory_is_a_transient_inline_folder() {
    let mut dom = VirtualDom::new(transient_folder_test_root);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("transient-folder"), "{html}");
    assert!(html.contains("/remote/new"), "{html}");
    assert!(html.contains("aria-label=\"Session name\""), "{html}");
}

#[test]
fn pending_creation_cannot_be_retargeted() {
    let mut ui = crate::app::Ui::new(DesktopSettings::default());
    let probe = psa_api::ProbeId("00000000-0000-0000-0000-000000000000".parse().unwrap());
    let first = crate::creation::CreationTarget {
        probe,
        directory: "/one".into(),
    };
    crate::creation::select_target(&mut ui, first.clone());
    ui.creation_name = "draft".into();
    ui.creation_pending = true;
    crate::creation::select_target(
        &mut ui,
        crate::creation::CreationTarget {
            probe,
            directory: "/two".into(),
        },
    );
    assert!(ui.creation_target == Some(first));
    assert_eq!(ui.creation_name, "draft");
}

fn empty_picker_test_root() -> Element {
    let ui = use_signal(|| {
        let mut state = crate::app::Ui::new(DesktopSettings::default());
        state.connection = "Connected".into();
        state
    });
    use_context_provider(|| ui);
    rsx! { crate::creation::CreateSessionPicker {} }
}

#[test]
fn picker_escape_dismisses_without_creating_a_folder() {
    use dioxus::{
        core::Mutation,
        html::{PlatformEventData, SerializedFormData},
    };
    use std::any::Any;
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(empty_picker_test_root);
    let mutations = dom.rebuild_to_vec();
    let button = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    click(&dom, button.0);
    let mutations = dom.render_immediate_to_vec();
    let dialog = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(dioxus_ssr::render(&dom).contains("No online probes available"));
    let path = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "input" => Some(*id),
            _ => None,
        })
        .unwrap();
    let input = Event::new(
        Rc::new(PlatformEventData::new(Box::new(SerializedFormData::new(
            "relative/path".into(),
            vec![],
        )))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("input", input, path);
    dom.render_immediate_to_vec();
    key(&dom, path, Key::Enter);
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("Enter an absolute path on the probe"));
    key(&dom, dialog, Key::Escape);
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("Choose working directory"), "{html}");
    assert!(!html.contains("Session name"), "{html}");
}

fn settings_test_root() -> Element {
    let bootstrap = use_context::<GuiBootstrap>();
    let initial = bootstrap.config.settings.clone();
    let ui = use_signal(move || crate::app::Ui::new(initial));
    use_context_provider(|| ui);
    rsx! { crate::views::Settings {} }
}

fn modal_settings_test_root() -> Element {
    let mut ui = use_signal(|| {
        let mut state = crate::app::Ui::new(DesktopSettings::default());
        state.settings_open = true;
        state
    });
    use_context_provider(|| ui);
    rsx! {
        div { onclick: move |_| ui.write().notice = Some("outer click".into()),
            onkeydown: move |_| ui.write().notice = Some("outer key".into()),
            if ui.read().settings_open { crate::views::Settings {} }
            if let Some(notice) = &ui.read().notice { "{notice}" }
        }
    }
}

fn click(dom: &VirtualDom, id: usize) {
    use dioxus::{
        core::ElementId,
        html::{PlatformEventData, SerializedMouseData},
    };
    use std::any::Any;
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("click", event, ElementId(id));
}

fn key(dom: &VirtualDom, id: dioxus::core::ElementId, key: Key) -> Event<dyn std::any::Any> {
    use dioxus::html::{Code, Location, PlatformEventData, SerializedKeyboardData};
    let data = SerializedKeyboardData::new(
        key,
        Code::Unidentified,
        Location::Standard,
        false,
        Modifiers::empty(),
        false,
    );
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>,
        true,
    );
    dom.runtime().handle_event("keydown", event.clone(), id);
    event
}

#[test]
fn settings_escape_closes_without_bubbling() {
    use dioxus::core::Mutation;
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let bootstrap = GuiBootstrap {
        config: GuiConfig {
            settings: DesktopSettings::default(),
        },
        platform: Rc::new(FakePlatform::default()),
    };
    let mut dom = VirtualDom::new(modal_settings_test_root).with_root_context(bootstrap);
    let mutations = dom.rebuild_to_vec();
    let control = mutations
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .nth(3)
        .unwrap();
    let dialog = mutations
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .last()
        .unwrap();
    assert!(mutations.edits.iter().any(|edit| matches!(edit, Mutation::NewEventListener { name, id } if name == "mounted" && *id == dialog)));
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("tabindex=\"-1\""), "{html}");
    let event = key(&dom, control, Key::Escape);
    assert!(!event.propagates());
    assert!(!event.default_action_enabled());
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("aria-label=\"Settings\""), "{html}");
    assert!(!html.contains("outer key"), "{html}");
}

#[tokio::test]
async fn create_session_escape_cancels_and_enter_blocks_duplicates() {
    use dioxus::{
        core::Mutation,
        html::{PlatformEventData, SerializedFormData},
    };
    use std::any::Any;
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(folder_create_test_root);
    let mutations = dom.rebuild_to_vec();
    let button = mutations
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .last()
        .unwrap();
    click(&dom, button.0);
    let mutations = dom.render_immediate_to_vec();
    let input_id = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "input" => Some(*id),
            _ => None,
        })
        .unwrap();
    let event = key(&dom, input_id, Key::Escape);
    assert!(!event.propagates());
    dom.render_immediate_to_vec();
    assert!(!dioxus_ssr::render(&dom).contains("aria-label=\"Session name\""));

    click(&dom, button.0);
    let mutations = dom.render_immediate_to_vec();
    let input_id = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "input" => Some(*id),
            _ => None,
        })
        .unwrap();
    let input = Event::new(
        Rc::new(PlatformEventData::new(Box::new(SerializedFormData::new(
            "Test".into(),
            vec![],
        )))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("input", input, input_id);
    dom.render_immediate_to_vec();
    key(&dom, input_id, Key::Enter);
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("disabled"));
    key(&dom, input_id, Key::Escape);
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("aria-label=\"Session name\""));
}

#[test]
fn settings_backdrop_closes_but_dialog_and_controls_do_not() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let bootstrap = GuiBootstrap {
        config: GuiConfig {
            settings: DesktopSettings::default(),
        },
        platform: Rc::new(FakePlatform::default()),
    };
    let mut dom = VirtualDom::new(modal_settings_test_root).with_root_context(bootstrap);
    dom.rebuild_in_place();
    // The root, backdrop, dialog, and navigation buttons receive these IDs
    // when mounted in this fixture.
    click(&dom, 4);
    click(&dom, 7);
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Settings\""), "{html}");
    assert!(html.contains("Model favorites"), "{html}");
    assert!(!html.contains("outer click"), "{html}");
    click(&dom, 3);
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("aria-label=\"Settings\""), "{html}");
    assert!(!html.contains("outer click"), "{html}");
}

#[test]
fn settings_renders_profile_cards_and_local_controls_for_remote_selection() {
    let mut settings = DesktopSettings::default();
    let remote = ConnectionProfile::new_remote("Remote test", ConnectionSettings::default());
    settings.active_profile_id = remote.id.clone();
    settings.profiles.push(remote);
    let bootstrap = GuiBootstrap {
        config: GuiConfig { settings },
        platform: Rc::new(FakePlatform::default()),
    };
    let mut dom = VirtualDom::new(settings_test_root).with_root_context(bootstrap);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("settings-nav"), "{html}");
    assert!(html.contains("profile-card active"), "{html}");
    assert!(html.contains("Remote test"), "{html}");
    assert!(html.contains("Selected"), "{html}");
    assert!(html.contains("Refresh status"), "{html}");
    assert!(html.contains("Restart"), "{html}");
    assert!(html.contains("Stop"), "{html}");
    assert!(html.contains("Save connection and reconnect"), "{html}");
    assert!(html.contains("Add remote profile"), "{html}");
    assert_eq!(html.matches("Delete profile").count(), 1, "{html}");
    assert!(html.contains("Delete Remote test"), "{html}");
}

#[test]
fn message_content_is_escaped_not_rendered_as_html() {
    let kind = EventType::Message(Message::text(
        MessageType::Human,
        "<script>alert('x')</script>",
    ));
    let html = dioxus_ssr::render_element(
        rsx! {crate::views::EventRow{kind,provisional:false,results:Rc::new(HashMap::new())}},
    );
    assert!(html.contains("&#60;script&#62;"), "{html}");
    assert!(!html.contains("<script>"));
    assert!(html.contains("Human"));
}

#[test]
fn binary_attachments_render_as_bounded_placeholders() {
    use llm_api::api::{ContentBlock, ImageBlock, MediaSource};
    let mut message = Message::text(MessageType::Agent, "An attachment:");
    message.content.push(ContentBlock::Image(ImageBlock {
        source: MediaSource::Inline {
            mime_type: "image/png".into(),
            data: vec![127; 64_000],
        },
        detail: None,
    }));
    let kind = EventType::Message(message);
    let html = dioxus_ssr::render_element(
        rsx! {crate::views::EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) }},
    );
    assert!(html.contains("Image attachment"));
    assert!(html.len() < 2000);
}

#[test]
fn unavailable_restored_targets_survive_connection_switches() {
    let target = TranscriptTarget {
        session_id: SessionId(99),
        agent_id: AgentId(42),
    };
    let mut settings = DesktopSettings::default();
    settings
        .ensure_current_server()
        .workspace
        .open(target, true);
    let remote = ConnectionProfile::new_remote("Remote", ConnectionSettings::default());
    let remote_id = remote.id.clone();
    settings.profiles.push(remote);
    let mut ui = crate::app::Ui::new(settings);
    assert_eq!(ui.workspace.active_tab().unwrap().target, target);
    ui.select_profile(remote_id);
    assert!(ui.workspace.active_tab().is_none());
    ui.select_profile(LOCAL_PROFILE_ID.into());
    assert_eq!(ui.workspace.active_tab().unwrap().target, target);
    assert!(ui.transcripts.is_empty());
    assert!(ui.drafts.is_empty());
}

#[test]
fn edge_drop_places_the_moved_tab_before_or_after_the_target_group() {
    for before in [true, false] {
        let mut workspace = Workspace::default();
        let first = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        };
        let second = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(2),
        };
        let view = workspace.open(first, true);
        workspace.open(second, true);
        let group = workspace.focused_group;
        crate::views::move_to_split(&mut workspace, view, group, SplitAxis::Horizontal, before);
        assert_eq!(workspace.groups().len(), 2);
        let index = if before { 0 } else { 1 };
        assert_eq!(workspace.groups()[index].tabs[0].target, first);
        assert_eq!(workspace.active_tab().unwrap().id, view);
    }
}

#[test]
fn divider_drag_respects_origin_bounds_and_invalid_geometry() {
    assert_eq!(crate::views::drag_ratio(350.0, 100.0, 1000.0), Some(0.25));
    assert_eq!(crate::views::drag_ratio(-1.0, 100.0, 1000.0), Some(0.1));
    assert_eq!(crate::views::drag_ratio(2000.0, 100.0, 1000.0), Some(0.9));
    assert_eq!(crate::views::drag_ratio(100.0, 0.0, 0.0), None);
    assert_eq!(crate::views::drag_ratio(f64::NAN, 0.0, 100.0), None);
}

#[test]
fn draft_promotion_keeps_preview_navigation_from_discarding_edits() {
    let mut ui = crate::app::Ui::new(DesktopSettings::default());
    let first = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(1),
    };
    let second = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(2),
    };
    ui.open(first, false);
    let view = ui.workspace.active_tab().unwrap().id;
    ui.workspace.promote(view);
    ui.drafts.insert(first, "unfinished".into());
    ui.open(second, false);
    assert_eq!(ui.workspace.groups()[0].tabs.len(), 2);
    assert_eq!(ui.drafts.get(&first).unwrap(), "unfinished");
}

#[test]
fn draft_root_tabs_are_not_persisted() {
    let mut ui = crate::app::Ui::new(DesktopSettings::default());
    let target = TranscriptTarget {
        session_id: SessionId(51),
        agent_id: crate::app::DRAFT_ROOT,
    };
    ui.open(target, true);
    ui.drafts.insert(target, "unfinished".into());
    let saved = ui.persisted();
    let restored = crate::app::Ui::new(saved);
    assert!(
        restored
            .workspace
            .groups()
            .iter()
            .all(|group| group.tabs.iter().all(|tab| tab.target != target))
    );
    assert!(!restored.last_agents.contains_key(&target.session_id));
    assert!(restored.drafts.is_empty());
}
