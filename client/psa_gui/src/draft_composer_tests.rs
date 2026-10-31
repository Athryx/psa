use super::*;

#[test]
fn root_command_stages_distinguish_completion_and_selection() {
    use DraftCommandStage::*;
    for input in ["/", "/ag", "/agent"] {
        assert_eq!(draft_command_stage(input, false, false), Some(Completion));
    }
    for input in ["/agent ", "/agent worker", "/agent\tworker"] {
        assert_eq!(
            draft_command_stage(input, false, false),
            Some(Configurations)
        );
    }
    for input in ["/m", "/model"] {
        assert_eq!(draft_command_stage(input, false, false), Some(Completion));
    }
    assert_eq!(
        draft_command_stage("/model search", false, false),
        Some(SessionModels)
    );
    assert_eq!(
        draft_command_stage("/effort", false, false),
        Some(Completion)
    );
    assert_eq!(
        draft_command_stage("/effort high", false, false),
        Some(SessionEfforts)
    );
    for input in ["//agent", "/agents", "hello"] {
        assert_eq!(draft_command_stage(input, false, false), None);
    }
    assert_eq!(draft_command_stage("", false, true), Some(Models));
    assert_eq!(draft_command_stage("/agent ", true, false), None);
    assert_eq!(draft_command_stage("", true, true), None);
}

#[tokio::test(flavor = "current_thread")]
async fn root_effort_picker_only_offers_current_model_options() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/eff");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Session default effort")
            && html.contains(">low<")
            && html.contains(">high<"),
        "{html}"
    );
    assert!(!html.contains(">medium<"), "{html}");
    root_key(&mut dom, textarea, Key::ArrowDown);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("aria-selected=true><span class=\"command-title\">high"),
        "{html}"
    );
    root_key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("<textarea value=\"\""));
    root_input(&mut dom, textarea, "/effort invalid");
    root_key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("/effort invalid"));
}

#[test]
fn effort_options_require_known_model_and_valid_catalog_entry() {
    let config: ModelConfig = serde_json::from_value(
        serde_json::json!({"model":{"provider":"Openrouter","api_name":"model"},"effort":"Low"}),
    )
    .unwrap();
    let mut model: ModelInfo = serde_json::from_value(serde_json::json!({
        "model":{"provider":"Openrouter","api_name":"model"}, "name":"model",
        "input_modalities":[], "context_window":null, "max_output_tokens":null,
        "cost":{"tiers":[]}, "reasoning":true,
        "reasoning_options":[{"effort":"Low","wire_effort":"Low"},{"effort":"High","wire_effort":"High"}],
        "authed":true, "pinned":false, "refreshed_at":"2026-01-01T00:00:00Z"
    })).unwrap();
    assert!(session_efforts(&[], Some(&config), "").is_empty());
    assert!(effort_model_config(&[], Some(&config), Effort::Low).is_none());
    assert!(effort_model_config(&[model.clone()], None, Effort::High).is_none());
    assert!(effort_model_config(&[model.clone()], Some(&config), Effort::Medium).is_none());
    let updated = effort_model_config(&[model.clone()], Some(&config), Effort::High).unwrap();
    assert_eq!(updated.model, config.model);
    assert_eq!(updated.effort, Effort::High);
    model.reasoning_options.clear();
    assert!(session_efforts(&[model.clone()], Some(&config), "").is_empty());
    assert!(effort_model_config(&[model], Some(&config), Effort::Low).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn effort_update_sends_session_model_and_selected_effort() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut data = Vec::new();
        let mut buffer = [0; 4096];
        let (end, len) = loop {
            let n = stream.read(&mut buffer).unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buffer[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..end]);
                let len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if data.len() >= end + 4 + len {
                    break (end + 4, len);
                }
            }
        };
        let headers = String::from_utf8_lossy(&data[..end]);
        assert!(headers.starts_with("PATCH /sessions/1 "), "{headers}");
        let request: serde_json::Value = serde_json::from_slice(&data[end..end + len]).unwrap();
        let response = serde_json::json!({"id":1,"parent_id":null,"name":"test","working_directory":"/tmp","probe_id":"00000000-0000-0000-0000-000000000001","model_config":request["model_config"],"busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        request
    });
    let mut dom = VirtualDom::new(|| rsx! { div {} });
    dom.rebuild_in_place();
    let mut settings = DesktopSettings::default();
    settings.active_profile_mut().connection.api_url = format!("http://{address}");
    let ui = dom.in_scope(ScopeId::ROOT, || Signal::new(Ui::new(settings)));
    let config: ModelConfig = serde_json::from_value(serde_json::json!({"model":{"provider":"Openrouter","api_name":"favorite"},"effort":"High"})).unwrap();
    let mut run = Box::pin(change_model(ui, SessionId(1), config));
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
    let request = server.join().unwrap();
    assert_eq!(request["model_config"]["model"]["provider"], "Openrouter");
    assert_eq!(request["model_config"]["model"]["api_name"], "favorite");
    assert_eq!(request["model_config"]["effort"], "High");
    assert!(ui.read().notice.is_some());
}

#[test]
fn root_model_labels_describe_equivalent_policies() {
    assert_eq!(
        draft_model_label(&ModelSelection::Parent),
        draft_model_label(&ModelSelection::Global)
    );
    let fixed = |effort| ModelSelection::Fixed {
        model: "provider/model".into(),
        effort,
    };
    assert_eq!(
        draft_model_label(&fixed(EffortSelection::Parent)),
        draft_model_label(&fixed(EffortSelection::Global))
    );
    assert_eq!(
        draft_model_label(&fixed(EffortSelection::Fixed(Effort::High))),
        "provider/model · high"
    );
}

use dioxus::{
    core::{ElementId, Mutation},
    html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
};
use std::{any::Any, rc::Rc};

fn root_fixture() -> Element {
    let mut state = Ui::new(DesktopSettings::default());
    let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
        "config": {"config_id":{"config_group":"Global","name":"root"}, "selectable":true,
          "models":["Parent","Global",{"Fixed":{"model":"provider/model","effort":"Global"}}], "default":false},
        "status":{"missing_components":[]}
    })).unwrap();
    state.sessions.push(
        serde_json::from_value(serde_json::json!({
            "id":1,"parent_id":null,"name":"test","working_directory":"/tmp",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":{"provider":"Openrouter","api_name":"favorite"},"effort":"Low"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    );
    let mut draft = crate::app::DraftRoot::default();
    draft.configs.push(status);
    draft.ready = true;
    state.draft_roots.insert(SessionId(1), draft);
    for (name, pinned) in [("Favorite", true), ("Other", false)] {
        state.models.push(
            serde_json::from_value(serde_json::json!({
                "model":{"provider":"Openrouter","api_name":name.to_lowercase()}, "name":name,
                "input_modalities":[], "context_window":null, "max_output_tokens":null,
                "cost":{"tiers":[]}, "reasoning":true, "reasoning_options":[{"effort":"Low","wire_effort":"Low"},{"effort":"High","wire_effort":"High"}],
                "authed":true, "pinned":pinned, "refreshed_at":"2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        );
    }
    let ui = use_signal(|| state);
    use_context_provider(|| ui);
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: crate::app::DRAFT_ROOT,
    };
    rsx! { DraftComposer { tab:Tab { id:ViewId(1), target, preview:false, last_focused:0 } } }
}

fn mount_root() -> (VirtualDom, ElementId) {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(root_fixture);
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

fn root_input(dom: &mut VirtualDom, id: ElementId, value: &str) {
    let data = SerializedFormData::new(value.into(), vec![]);
    dom.runtime().handle_event(
        "input",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
            true,
        ),
        id,
    );
    dom.render_immediate_to_vec();
}

fn root_key(dom: &mut VirtualDom, id: ElementId, key: Key) {
    let data = SerializedKeyboardData::new(
        key,
        Code::Unidentified,
        Location::Standard,
        false,
        Modifiers::empty(),
        false,
    );
    dom.runtime().handle_event(
        "keydown",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
            true,
        ),
        id,
    );
    dom.render_immediate_to_vec();
}

#[test]
fn root_completion_and_two_step_selection() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/ag");
    assert!(dioxus_ssr::render(&dom).contains("aria-label=\"Slash commands\""));
    root_key(&mut dom, textarea, Key::Tab);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/agent "), "{html}");
    assert!(html.contains("Choose a root agent"), "{html}");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("Choose the root agent model policy"),
        "{html}"
    );
    assert_eq!(
        html.matches("Session default model and effort").count(),
        1,
        "{html}"
    );
    root_key(&mut dom, textarea, Key::ArrowDown);
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        !html.contains("Choose the root agent model policy"),
        "{html}"
    );
    assert!(!html.contains("Model: provider/model"), "{html}");
    // Reselecting the configuration still exposes its model policies.
    root_input(&mut dom, textarea, "/agent ");
    root_key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("Choose the root agent model policy"));
}

#[test]
fn root_info_shows_unknown_usage_before_agent_creation() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/info");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Agent information\""), "{html}");
    assert!(
        html.contains("Input tokens") && html.contains("Unknown"),
        "{html}"
    );
    assert!(html.contains("<textarea value=\"\""), "{html}");
}

#[test]
fn root_composer_is_blank_and_refresh_is_a_command() {
    let (mut dom, textarea) = mount_root();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("<textarea value=\"\""), "{html}");
    assert!(!html.contains("placeholder="), "{html}");
    assert!(html.contains("aria-label=\"Send first message\""), "{html}");
    assert!(!html.contains("Refresh configurations"), "{html}");
    root_input(&mut dom, textarea, "/ref");
    assert!(dioxus_ssr::render(&dom).contains("/refresh"));
    root_key(&mut dom, textarea, Key::Tab);
    assert!(dioxus_ssr::render(&dom).contains("value=\"/refresh\""));
}

#[tokio::test(flavor = "current_thread")]
async fn root_refresh_executes_from_completion_and_is_not_sent_as_text() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/refresh");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("<textarea value=\"\""), "{html}");
    assert!(!html.contains("aria-label=\"Slash commands\""), "{html}");
}

#[test]
fn root_escape_and_literal_slash() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/agent");
    root_key(&mut dom, textarea, Key::Enter);
    assert!(dioxus_ssr::render(&dom).contains("Choose a root agent"));
    root_key(&mut dom, textarea, Key::Escape);
    assert!(!dioxus_ssr::render(&dom).contains("Choose a root agent"));
    root_input(&mut dom, textarea, "//agent");
    assert!(!dioxus_ssr::render(&dom).contains("aria-label=\"Slash commands\""));
}

#[tokio::test(flavor = "current_thread")]
async fn root_supports_session_model_command_without_changing_root_policy() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/");
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/model") && html.contains("/agent"), "{html}");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/model "), "{html}");
    assert!(
        html.contains("Session default model (not individual agents)"),
        "{html}"
    );
    assert!(html.contains("Favorite"), "{html}");
    assert!(!html.contains(">Other<"), "{html}");

    root_input(&mut dom, textarea, "/model fav");
    assert!(dioxus_ssr::render(&dom).contains("Favorite"));
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(
        !html.contains("Session default model (not individual agents)"),
        "{html}"
    );
    assert!(!html.contains("Root agent:"), "{html}");

    root_input(&mut dom, textarea, "/");
    root_key(&mut dom, textarea, Key::ArrowDown);
    root_key(&mut dom, textarea, Key::Tab);
    assert!(dioxus_ssr::render(&dom).contains("Choose a root agent"));
}
