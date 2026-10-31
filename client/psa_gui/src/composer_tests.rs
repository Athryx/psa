use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
};
use std::{any::Any, rc::Rc};

#[test]
fn menu_scroll_uses_nearest_edge_and_leaves_visible_items_alone() {
    assert_eq!(menu_scroll_delta(100.0, 200.0, 120.0, 30.0), 0.0);
    assert_eq!(menu_scroll_delta(100.0, 200.0, 80.0, 30.0), -20.0);
    assert_eq!(menu_scroll_delta(100.0, 200.0, 280.0, 30.0), 10.0);
    assert_eq!(menu_scroll_delta(100.0, 200.0, 100.0, 200.0), 0.0);
}

fn width_fixture() -> Element {
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        state.settings.preferences.conversation_max_width_ch = 110;
        state.settings.preferences.transcript_font_size = 18;
        state
    });
    use_context_provider(|| ui);
    let tab = |agent_id| Tab {
        id: ViewId(agent_id),
        target: TranscriptTarget {
            session_id: SessionId(1),
            agent_id: psa_api::AgentId(agent_id),
        },
        preview: false,
        last_focused: 0,
    };
    rsx! {
        TranscriptView { tab: tab(2), active: false }
        TranscriptView { tab: tab(crate::app::DRAFT_ROOT.0), active: false }
    }
}

#[test]
fn configured_width_styles_transcripts_and_both_composers() {
    let mut dom = VirtualDom::new(width_fixture);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("font-size:18px;--conversation-max-width:110rch"));
    assert_eq!(
        html.matches("--conversation-max-width:110rch").count(),
        4,
        "{html}"
    );
    let css = crate::style::CSS;
    assert_eq!(
        css.matches("var(--conversation-max-width, 85rch)").count(),
        4
    );
    assert!(css.contains("max(16px,calc((100% - var(--conversation-max-width, 85rch))/2))"));
}

fn composer_fixture() -> Element {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: psa_api::AgentId(2),
    };
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        // Selection should not send a request to a running local server during tests.
        state.settings.active_profile_mut().connection.api_url = "invalid-url".into();
        state.sessions.push(serde_json::from_value(serde_json::json!({
            "id":1,"parent_id":null,"name":"test","working_directory":"/tmp",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":{"provider":"Openrouter","api_name":"alpha"},"effort":"Low"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap());
        state.agents.insert(SessionId(1), vec![serde_json::from_value(serde_json::json!({
            "id":2,"definition_id":1,"session_id":1,"parent_id":null,"subagent_parent_id":null,
            "name":"worker","hidden":false,"interactive":true,"state":"Idle",
            "usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130,"cached_input_tokens":25,"reasoning_output_tokens":10},
            "next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap()]);
        for name in ["Alpha", "Beta"] {
            state.models.push(
                serde_json::from_value(serde_json::json!({
                    "model": {"provider": "Openrouter", "api_name": name.to_lowercase()},
                    "name": name,
                    "input_modalities": [], "context_window": null, "max_output_tokens": null,
                    "cost": {"tiers": []}, "reasoning": true, "reasoning_options": [{"effort":"Low","wire_effort":"Low"},{"effort":"High","wire_effort":"High"}],
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

fn info_fixture() -> Element {
    let open = use_signal(|| true);
    rsx! { if open() { AgentInfo { name:"worker".to_string(), template:None, definition_id:None, path:None, probe:None, model:None, effort:None, usage:None, open } } }
}

fn info_cache_write_fixture() -> Element {
    let open = use_signal(|| true);
    let usage = AgentUsage {
        cache_write_tokens: Some(42),
        ..Default::default()
    };
    rsx! { AgentInfo { name:"worker".to_string(), template:None, definition_id:None, path:None, probe:None, model:None, effort:None, usage:Some(usage), open } }
}

#[test]
fn info_dialog_closes_on_escape_and_backdrop() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(info_fixture);
    let edits = dom.rebuild_to_vec();
    let dialog = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .unwrap();
    key(&mut dom, dialog, Key::Escape);
    assert!(!dioxus_ssr::render(&dom).contains("Agent information"));

    let mut dom = VirtualDom::new(info_fixture);
    let edits = dom.rebuild_to_vec();
    let backdrop = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    let data = dioxus::html::SerializedMouseData::default();
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("click", event, backdrop);
    dom.render_immediate_to_vec();
    assert!(!dioxus_ssr::render(&dom).contains("Agent information"));
}

#[test]
fn info_command_shows_agent_usage_without_sending() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/info");
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Agent information\""), "{html}");
    for value in [
        "worker",
        "Input tokens",
        "100",
        "Output tokens",
        "30",
        "Total tokens",
        "130",
        "Cached",
        "Cache hit rate",
        "25.0%",
        "Uncached",
        "75",
        "Thinking",
        "Non-thinking",
        "20",
        "10",
    ] {
        assert!(html.contains(value), "missing {value}: {html}");
    }
    assert!(html.contains("<textarea value=\"\""), "{html}");
}

#[test]
fn info_token_breakdown_preserves_unknown_and_rejects_inconsistent_counts() {
    assert_eq!(token_difference(Some(100), Some(25)), Some(75));
    assert_eq!(token_difference(None, Some(25)), None);
    assert_eq!(token_difference(Some(100), None), None);
    assert_eq!(token_difference(Some(10), Some(11)), None);
    let mut dom = VirtualDom::new(info_fixture);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Unknown"), "{html}");
    assert!(!html.contains("Cache write"), "{html}");
    let mut dom = VirtualDom::new(info_cache_write_fixture);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Cache write") && html.contains("42"),
        "{html}"
    );
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
fn escape_dismisses_popup_before_interrupting_and_preserves_draft() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/effort ");
    assert!(dioxus_ssr::render(&dom).contains("Session default effort"));
    key(&mut dom, textarea, Key::Escape);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("Session default effort"), "{html}");
    assert!(html.contains("value=\"/effort \""), "{html}");
    assert!(!html.contains(">Interrupt</button>"), "{html}");
}

#[tokio::test]
async fn effort_picker_filters_navigates_and_selects() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/eff");
    assert!(dioxus_ssr::render(&dom).contains("/effort"));
    key(&mut dom, textarea, Key::Tab);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Session default effort")
            && html.contains(">low<")
            && html.contains(">high<"),
        "{html}"
    );
    assert!(!html.contains(">medium<"), "{html}");
    key(&mut dom, textarea, Key::ArrowDown);
    assert!(
        dioxus_ssr::render(&dom)
            .contains("aria-selected=\"true\"><span class=\"command-title\">high")
    );
    key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("<textarea value=\"\""));
    input(&mut dom, textarea, "/effort med");
    assert!(dioxus_ssr::render(&dom).contains("No matching effort options"));
    key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("/effort med"));
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
