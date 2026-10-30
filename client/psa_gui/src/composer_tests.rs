use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
};
use std::{any::Any, rc::Rc};

fn composer_fixture() -> Element {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: psa_api::AgentId(2),
    };
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        // Selection should not send a request to a running local server during tests.
        state.settings.active_profile_mut().connection.api_url = "invalid-url".into();
        for name in ["Alpha", "Beta"] {
            state.models.push(
                serde_json::from_value(serde_json::json!({
                    "model": {"provider": "Openrouter", "api_name": name.to_lowercase()},
                    "name": name,
                    "input_modalities": [], "context_window": null, "max_output_tokens": null,
                    "cost": {"tiers": []}, "reasoning": false, "reasoning_options": [],
                    "authed": true, "pinned": true, "refreshed_at": "2024-01-01T00:00:00Z"
                }))
                .unwrap(),
            );
        }
        state
    });
    use_context_provider(|| ui);
    rsx! { Composer { tab: Tab { id: ViewId(1), target, preview: false, last_focused: 0 } } }
}

fn mount() -> (VirtualDom, ElementId) {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(composer_fixture);
    let edits = dom.rebuild_to_vec();
    let textarea = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .expect("textarea key listener");
    (dom, textarea)
}

fn input(dom: &mut VirtualDom, id: ElementId, value: &str) {
    let data = SerializedFormData::new(value.into(), vec![]);
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("input", event, id);
    dom.render_immediate_to_vec();
}

fn key(dom: &mut VirtualDom, id: ElementId, key: Key) {
    let data = SerializedKeyboardData::new(
        key,
        Code::Unidentified,
        Location::Standard,
        false,
        Modifiers::empty(),
        false,
    );
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("keydown", event, id);
    dom.render_immediate_to_vec();
}

#[test]
fn composer_starts_blank_with_inline_send() {
    let (dom, _) = mount();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("<textarea value=\"\""), "{html}");
    assert!(!html.contains("placeholder="), "{html}");
    assert!(html.contains("aria-label=\"Send steer\""), "{html}");
    assert!(html.contains("class=\"composer-mirror\""), "{html}");
}

#[test]
fn completion_accepts_tab_and_enter_and_escape_dismisses() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/mo");
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/model"), "{html}");
    key(&mut dom, textarea, Key::Tab);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Session default model (not individual agents)"),
        "{html}"
    );
    assert!(html.contains("<textarea value=\"/model \""), "{html}");

    input(&mut dom, textarea, "/model");
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Session default model (not individual agents)"),
        "{html}"
    );
    assert!(html.contains("<textarea value=\"/model \""), "{html}");

    input(&mut dom, textarea, "/mo");
    key(&mut dom, textarea, Key::Escape);
    assert!(!dioxus_ssr::render(&dom).contains("aria-label=\"Slash commands\""));
    input(&mut dom, textarea, "/m");
    assert!(dioxus_ssr::render(&dom).contains("aria-label=\"Slash commands\""));
}

#[tokio::test]
async fn model_picker_navigates_and_enter_selects() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/model ");
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Alpha") && html.contains("Beta"), "{html}");
    key(&mut dom, textarea, Key::ArrowDown);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("aria-selected=\"true\"><span class=\"command-title\">Beta"),
        "{html}"
    );
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("aria-label=\"Slash commands\""), "{html}");
    assert!(html.contains("<textarea value=\"\""), "{html}");
}

#[test]
fn agent_command_is_explained_and_literal_slashes_do_not_show_commands() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/agent worker");
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("only available before creating the root agent"),
        "{html}"
    );
    assert!(!html.contains("aria-label=\"Slash commands\""), "{html}");
    input(&mut dom, textarea, "//agent worker");
    let html = dioxus_ssr::render(&dom);
    assert!(
        !html.contains("only available before creating the root agent"),
        "{html}"
    );
    assert!(!html.contains("aria-label=\"Slash commands\""), "{html}");
}
