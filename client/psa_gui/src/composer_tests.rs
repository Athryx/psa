use super::*;
use dioxus::{
    core::{ElementId, Mutation},
    html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
};
use std::{any::Any, rc::Rc};

thread_local! {
    static COMPOSER_WORK: std::cell::RefCell<std::collections::HashMap<TranscriptTarget, [usize; 3]>> = std::cell::RefCell::new(std::collections::HashMap::new());
    static KEY_HANDLERS: std::cell::RefCell<std::collections::HashMap<ViewId, EventHandler<InputKeyEvent>>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

pub(super) fn record_key_handler(view: ViewId, handler: EventHandler<InputKeyEvent>) {
    KEY_HANDLERS.with(|handlers| {
        handlers.borrow_mut().insert(view, handler);
    });
}

pub(super) fn record_work(target: TranscriptTarget, kind: usize) {
    COMPOSER_WORK.with(|work| work.borrow_mut().entry(target).or_default()[kind] += 1);
}

fn work(target: TranscriptTarget) -> [usize; 3] {
    COMPOSER_WORK.with(|work| work.borrow().get(&target).copied().unwrap_or_default())
}

#[test]
fn question_submission_reads_native_form_values_before_queued_edits() {
    let mut answers = DraftStore::<usize>::default();
    answers.insert(0, "stale Rust answer".into());
    answers.insert(1, "older second answer".into());
    let form = dioxus::html::FormData::new(SerializedFormData::new(
        String::new(),
        vec![
            dioxus::html::SerializedFormObject {
                key: "answer-0".into(),
                text: Some("fresh native answer".into()),
                file: None,
            },
            dioxus::html::SerializedFormObject {
                key: "answer-1".into(),
                text: Some("fresh second answer".into()),
                file: None,
            },
        ],
    ));
    assert_eq!(
        question_answer_values(&form, 2, &answers),
        ["fresh native answer", "fresh second answer"]
    );
}

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
            agent_id: psa::api::AgentId(agent_id),
        },
        kind: TabKind::Transcript,
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
    assert!(html.contains("font-size:var(--psa-transcript-font-size,18px);--conversation-max-width:var(--psa-conversation-width,110rch)"));
    assert_eq!(
        html.matches("--conversation-max-width:var(--psa-conversation-width,110rch)")
            .count(),
        4,
        "{html}"
    );
    let css = crate::style::CSS;
    // The composer padding, plus the slash menu and command prompt edges.
    assert_eq!(
        css.matches("var(--conversation-max-width, 85rch)").count(),
        6
    );
    assert!(css.contains("max(16px,calc((100% - var(--conversation-max-width, 85rch))/2))"));
}

fn composer_fixture() -> Element {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: psa::api::AgentId(2),
    };
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        // Selection should not send a request to a running local server during tests.
        state.settings.active_profile_mut().connection.api_url = "invalid-url".into();
        state.sessions.push(
            serde_json::from_value(serde_json::json!({
                "id":1,"parent_id":null,"name":"test","working_directory":"/tmp",
                "probe_id":"00000000-0000-0000-0000-000000000001",
                "model_config":{"model":"openrouter/alpha","effort":"Low"},
                "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        );
        state.agents.insert(SessionId(1), vec![serde_json::from_value(serde_json::json!({
            "id":2,"definition_id":1,"session_id":1,"parent_id":null,"subagent_parent_id":null,
            "name":"worker","hidden":false,"interactive":true,"state":"Idle",
            "usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130,"cached_input_tokens":25,"reasoning_output_tokens":10},
            "next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap()].into());
        for name in ["Alpha", "Beta"] {
            state.models.push(
                serde_json::from_value(serde_json::json!({
                    "model": format!("openrouter/{}", name.to_lowercase()),
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
    rsx! { Composer { tab: Tab { id: ViewId(1), target, kind: TabKind::Transcript, preview: false, last_focused: 0 } } }
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

fn composer_ui(dom: &VirtualDom) -> Signal<Ui> {
    dom.in_scope(ScopeId::APP, consume_context::<Signal<Ui>>)
}

fn composer_draft(dom: &VirtualDom) -> String {
    composer_ui(dom)
        .peek()
        .drafts
        .get(&TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(2),
        })
        .cloned()
        .unwrap_or_default()
}

pub(super) fn flush_dom(dom: &mut VirtualDom) {
    // BrowserInput's headless mutation ACK runs as an effect after diffing.
    for _ in 0..4 {
        dom.render_immediate_to_vec();
    }
}

fn input(dom: &mut VirtualDom, id: ElementId, value: &str) {
    input_without_render(dom, id, value);
    flush_dom(dom);
}

fn input_without_render(dom: &mut VirtualDom, id: ElementId, value: &str) {
    let data = SerializedFormData::new(value.into(), vec![]);
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("input", event, id);
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
    flush_dom(dom);
}

fn info_fixture() -> Element {
    let open = use_signal(|| true);
    rsx! { if open() { AgentInfo { name:"worker".to_string(), template:None, definition_id:None, path:None, probe:None, model:None, effort:None, usage:None, open } } }
}

fn info_cache_write_fixture() -> Element {
    let open = use_signal(|| true);
    let usage = Usage {
        cache_write_tokens: Some(42),
        ..Usage::zero()
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
    assert!(composer_draft(&dom).is_empty(), "{html}");
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
    assert!(composer_draft(&dom).is_empty(), "{html}");
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
    assert_eq!(composer_draft(&dom), "/model ");

    input(&mut dom, textarea, "/model");
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Session default model (not individual agents)"),
        "{html}"
    );
    assert_eq!(composer_draft(&dom), "/model ");

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
    assert!(composer_draft(&dom).is_empty(), "{html}");
}

#[test]
fn escape_dismisses_popup_before_interrupting_and_preserves_draft() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/effort ");
    assert!(dioxus_ssr::render(&dom).contains("Session default effort"));
    key(&mut dom, textarea, Key::Escape);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("Session default effort"), "{html}");
    assert_eq!(composer_draft(&dom), "/effort ");
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
    assert!(composer_draft(&dom).is_empty());
    input(&mut dom, textarea, "/effort med");
    assert!(dioxus_ssr::render(&dom).contains("No matching effort options"));
    key(&mut dom, textarea, Key::Enter);
    assert_eq!(composer_draft(&dom), "/effort med");
}

fn two_target_fixture() -> Element {
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        for agent_id in [AgentId(2), AgentId(3)] {
            let target = TranscriptTarget {
                session_id: SessionId(1),
                agent_id,
            };
            state.transcripts.insert(
                target,
                Transcript::new(target.session_id, Some(agent_id)).into(),
            );
        }
        state
    });
    use_context_provider(|| ui);
    rsx! {
        for id in [2, 3] {
            Composer { key:"{id}", tab:Tab {
                id:ViewId(id), target:TranscriptTarget { session_id:SessionId(1), agent_id:AgentId(id) },
                kind:TabKind::Transcript, preview:false, last_focused:0,
            } }
        }
    }
}

#[test]
fn composer_work_is_scoped_to_its_target_and_typing_does_not_project_status() {
    COMPOSER_WORK.with(|work| work.borrow_mut().clear());
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(two_target_fixture);
    let edits = dom.rebuild_to_vec();
    dom.render_immediate_to_vec();
    let textarea = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .unwrap();
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(2),
    };
    let other = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(3),
    };
    let initial = work(target);

    composer_ui(&dom)
        .write()
        .drafts
        .insert(other, "other draft".into());
    dom.render_immediate_to_vec();
    assert_eq!(
        work(target),
        initial,
        "another draft must not render this editor"
    );
    composer_ui(&dom).write().transcripts.insert(
        other,
        Transcript::new(other.session_id, Some(other.agent_id)).into(),
    );
    dom.render_immediate_to_vec();
    assert_eq!(
        work(target),
        initial,
        "another transcript must not render or project this editor"
    );
    composer_ui(&dom).write().transcripts.insert(
        target,
        Transcript::new(target.session_id, Some(target.agent_id)).into(),
    );
    dom.render_immediate_to_vec();
    assert_eq!(
        work(target)[0],
        initial[0],
        "unchanged selected-target status must not render the editor"
    );
    let status_projections = work(target)[1];

    input(&mut dom, textarea, "typed locally");
    let after_typing = work(target);
    assert!(after_typing[0] > initial[0]);
    assert_eq!(
        after_typing[1], status_projections,
        "typing must not reproject transcript status"
    );
    assert_eq!(
        after_typing[2], 0,
        "ordinary text must not enumerate model candidates"
    );
}

fn duplicate_target_fixture() -> Element {
    let ui = use_signal(|| Ui::new(DesktopSettings::default()));
    use_context_provider(|| ui);
    rsx! {
        for view in [2, 3] {
            Composer { key:"{view}", tab:Tab {
                id:ViewId(view), target:TranscriptTarget { session_id:SessionId(1), agent_id:AgentId(2) },
                kind:TabKind::Transcript, preview:false, last_focused:0,
            } }
        }
    }
}

#[test]
fn peer_edit_expires_dismissal_before_tab_on_a_duplicate_editor() {
    KEY_HANDLERS.with(|handlers| handlers.borrow_mut().clear());
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(duplicate_target_fixture);
    let edits = dom.rebuild_to_vec();
    let inputs: Vec<_> = edits
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(inputs.len(), 2);
    input(&mut dom, inputs[0], "/mo");
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(2),
    };
    let original = composer_ui(&dom)
        .peek()
        .drafts
        .snapshot(&target)
        .unwrap()
        .revision;
    key(&mut dom, inputs[0], Key::Escape);
    input(&mut dom, inputs[1], "/mo");

    // Native duplicate editors already share DOM text/clock before Rust receives
    // an edit notification. Deliver that key packet without an original-editor
    // onedit reset, to exercise clock-bound local dismissal itself.
    let mut snapshot = composer_ui(&dom)
        .peek()
        .drafts
        .snapshot(&target)
        .unwrap()
        .clone();
    snapshot.revision.mount = original.mount;
    snapshot.revision.sequence = original.sequence;
    // Headless input events use clock zero; native shared edits advance it.
    snapshot.revision.clock = original.clock + 1;
    snapshot.cause = crate::input::SnapshotCause::Key;
    let handler = KEY_HANDLERS.with(|handlers| handlers.borrow()[&ViewId(2)]);
    dom.in_scope(ScopeId::ROOT, || {
        handler.call(InputKeyEvent::from_snapshot(snapshot, Key::Tab))
    });
    assert_eq!(composer_draft(&dom), "/model ");
}

#[test]
fn key_completion_uses_input_snapshot_before_the_next_render() {
    let (mut dom, textarea) = mount();
    input_without_render(&mut dom, textarea, "/mo");
    assert!(
        composer_ui(&dom)
            .peek()
            .drafts
            .mutation(&TranscriptTarget {
                session_id: SessionId(1),
                agent_id: AgentId(2),
            })
            .is_none(),
        "ordinary edits must not echo a replacement"
    );
    key(&mut dom, textarea, Key::Tab);
    assert_eq!(composer_draft(&dom), "/model ");
    assert!(dioxus_ssr::render(&dom).contains("Session default model (not individual agents)"));
}

#[test]
fn enter_rebuilds_filtered_candidates_from_unrendered_text() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/model a");
    input_without_render(&mut dom, textarea, "/model missing");
    key(&mut dom, textarea, Key::Enter);
    assert_eq!(composer_draft(&dom), "/model missing");
    assert!(dioxus_ssr::render(&dom).contains("No matching models"));
}

#[test]
fn enter_rebuilds_model_candidates_from_current_metadata() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/model alpha");
    composer_ui(&dom)
        .write()
        .models
        .retain(|model| model.name != "Alpha");
    key(&mut dom, textarea, Key::Enter);
    assert_eq!(composer_draft(&dom), "/model alpha");
    assert!(dioxus_ssr::render(&dom).contains("No matching models"));
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

#[test]
fn rename_command_opens_a_prefilled_prompt_above_the_composer() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/ren");
    assert!(dioxus_ssr::render(&dom).contains("/rename"));
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Rename session\""), "{html}");
    assert!(html.contains("value=\"test\""), "{html}");
    assert!(composer_draft(&dom).is_empty(), "{html}");
}

#[test]
fn rename_with_a_name_renames_without_opening_the_prompt() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/rename New name");
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("aria-label=\"Rename session\""), "{html}");
    assert!(composer_draft(&dom).is_empty(), "{html}");
}

#[test]
fn compact_completes_with_room_for_instructions() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/comp");
    assert!(dioxus_ssr::render(&dom).contains("/compact"));
    key(&mut dom, textarea, Key::Tab);
    assert_eq!(composer_draft(&dom), "/compact ");
}

#[test]
fn refresh_is_a_command_for_existing_agents_and_is_not_sent_as_text() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/ref");
    assert!(dioxus_ssr::render(&dom).contains("/refresh"));
    input(&mut dom, textarea, "/refresh");
    key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("only available before creating"), "{html}");
    assert!(composer_draft(&dom).is_empty(), "{html}");
}

#[test]
fn fork_is_a_command_and_clears_the_draft() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/fo");
    assert!(dioxus_ssr::render(&dom).contains("/fork"));
    input(&mut dom, textarea, "/fork other approach");
    key(&mut dom, textarea, Key::Enter);
    assert!(composer_draft(&dom).is_empty());
}

#[test]
fn escape_closes_the_rename_prompt_without_renaming() {
    let (mut dom, textarea) = mount();
    input(&mut dom, textarea, "/rename");
    // Press Enter by hand to keep the mutations that mount the prompt input.
    let data = SerializedKeyboardData::new(
        Key::Enter,
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
    dom.runtime().handle_event("keydown", event, textarea);
    let mut prompt_input = None;
    for _ in 0..4 {
        for edit in dom.render_immediate_to_vec().edits {
            if let Mutation::NewEventListener { name, id } = edit
                && name == "keydown"
                && id != textarea
            {
                prompt_input = Some(id);
            }
        }
    }
    assert!(dioxus_ssr::render(&dom).contains("aria-label=\"Rename session\""));

    key(
        &mut dom,
        prompt_input.expect("prompt input key listener"),
        Key::Escape,
    );
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("aria-label=\"Rename session\""), "{html}");
}

#[test]
fn picker_options_tab_complete_to_their_typed_command_form() {
    let option = |action| MenuOption {
        title: "stop".into(),
        description: String::new(),
        action,
        completion: None,
    };
    let sandbox = option(MenuAction::Sandbox(SandboxAction::Stop));
    assert_eq!(
        menu_completion(DraftCommandStage::SandboxActions, &sandbox).as_deref(),
        Some("/sandbox stop")
    );
    let effort = option(MenuAction::SessionEffort(Effort::High));
    assert_eq!(
        menu_completion(DraftCommandStage::SessionEfforts, &effort).as_deref(),
        Some("/effort stop")
    );
    let refresh = COMMANDS
        .iter()
        .find(|c| c.id == CommandId::Refresh)
        .unwrap();
    let sandbox_command = COMMANDS
        .iter()
        .find(|c| c.id == CommandId::Sandbox)
        .unwrap();
    assert!(sandbox_command.picker && !refresh.picker);
    assert_eq!(
        menu_completion(
            DraftCommandStage::Completion,
            &option(MenuAction::Command(*refresh))
        )
        .as_deref(),
        Some("/refresh")
    );
    assert_eq!(
        menu_completion(
            DraftCommandStage::Completion,
            &option(MenuAction::Command(*sandbox_command))
        )
        .as_deref(),
        Some("/sandbox ")
    );
    assert_eq!(menu_completion(DraftCommandStage::Models, &effort), None);
}
