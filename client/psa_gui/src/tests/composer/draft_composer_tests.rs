use super::*;

#[test]
fn argument_menus_are_found_by_the_typed_command() {
    for input in ["/agent ", "/agent worker", "/agent\tworker"] {
        assert_eq!(argument_command(input), Some(CommandId::Agent), "{input}");
    }
    assert_eq!(
        argument_command("/model search"),
        Some(CommandId::SessionModel)
    );
    assert_eq!(
        argument_command("/effort high"),
        Some(CommandId::SessionEffort)
    );
    for input in ["/", "/agent", "/model", "//agent ", "/unknown x", "hello"] {
        assert_eq!(argument_command(input), None, "{input}");
    }
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
    assert!(root_draft(&dom).is_empty());
    root_input(&mut dom, textarea, "/effort invalid");
    root_key(&mut dom, textarea, Key::Enter);
    assert_eq!(root_draft(&dom), "/effort invalid");
}

#[test]
fn effort_options_require_known_model_and_valid_catalog_entry() {
    let config: ModelConfig =
        serde_json::from_value(serde_json::json!({"model":"openrouter/model","effort":"Low"}))
            .unwrap();
    let mut model: ModelInfo = serde_json::from_value(serde_json::json!({
        "model":"openrouter/model", "name":"model",
        "input_modalities":[], "context_window":null, "max_output_tokens":null,
        "cost":{"tiers":[]}, "reasoning":true,
        "reasoning_options":[{"effort":"Low","wire_effort":"Low"},{"effort":"High","wire_effort":"High"}],
        "authed":true, "pinned":false, "refreshed_at":"2026-01-01T00:00:00Z"
    })).unwrap();
    assert!(session_efforts(&[], Some(&config)).is_empty());
    assert!(build_effort_config(&[], Some(&config), Effort::Low).is_none());
    assert!(build_effort_config(&[model.clone()], None, Effort::High).is_none());
    assert!(build_effort_config(&[model.clone()], Some(&config), Effort::Medium).is_none());
    let updated = build_effort_config(&[model.clone()], Some(&config), Effort::High).unwrap();
    assert_eq!(updated.model, config.model);
    assert_eq!(updated.effort, Effort::High);
    model.reasoning_options.clear();
    assert!(session_efforts(&[model.clone()], Some(&config)).is_empty());
    assert!(build_effort_config(&[model], Some(&config), Effort::Low).is_none());
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
        let response = serde_json::json!({"id":1,"forked_from":null,"name":"test","working_directory":"/tmp","probe_id":"00000000-0000-0000-0000-000000000001","model_config":request["model_config"],"busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        request
    });
    let mut dom = VirtualDom::new(|| rsx! { div {} });
    dom.rebuild_in_place();
    let mut settings = DesktopSettings::default();
    settings.active_profile_mut().connection.api_url = format!("http://{address}");
    let ui = dom.in_scope(ScopeId::ROOT, || Signal::new(Ui::new(settings)));
    let config: ModelConfig =
        serde_json::from_value(serde_json::json!({"model":"openrouter/favorite","effort":"High"}))
            .unwrap();
    let mut run = Box::pin(change_model(ui, SessionId(1), config));
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
    let request = server.join().unwrap();
    assert_eq!(request["model_config"]["model"], "openrouter/favorite");
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
        model: "openrouter/model".parse().unwrap(),
        effort,
    };
    assert_eq!(
        draft_model_label(&fixed(EffortSelection::Parent)),
        draft_model_label(&fixed(EffortSelection::Global))
    );
    assert_eq!(
        draft_model_label(&fixed(EffortSelection::Fixed(Effort::High))),
        "openrouter/model · high"
    );
}

use dioxus::{
    core::{ElementId, Mutation},
    html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
};
use std::{any::Any, rc::Rc};

thread_local! {
    static SUBMISSION_TEST_URL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    static HIDE_SUBMISSION_COMPOSER: std::cell::Cell<Option<Signal<bool>>> = const { std::cell::Cell::new(None) };
}

fn submission_fixture() -> Element {
    let mut settings = DesktopSettings::default();
    settings.active_profile_mut().connection.api_url =
        SUBMISSION_TEST_URL.with(|url| url.borrow().clone());
    let mut state = Ui::new(settings);
    let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
        "config": {"config_id":{"config_group":"Global","name":"root"}, "selectable":true,
          "models":["Global"], "default":true},
        "status":{"missing_components":[]}
    }))
    .unwrap();
    let mut draft = crate::app::DraftRoot::default();
    draft.ready = true;
    draft.configs.push(status.clone());
    draft.select_config(status.config.config_id);
    state.draft_roots.insert(SessionId(1), draft.into());
    let ui = use_signal(|| state);
    let hidden = use_signal(|| false);
    HIDE_SUBMISSION_COMPOSER.with(|slot| slot.set(Some(hidden)));
    use_context_provider(|| ui);
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: crate::app::DRAFT_ROOT,
    };
    let promoted = !ui
        .read()
        .agents
        .get(&SessionId(1))
        .is_none_or(|agents| agents.is_empty());
    let uncertain = !ui.read().uncertain_submissions.is_empty();
    let pending = !ui.read().pending_submissions.is_empty();
    if promoted || hidden() {
        rsx! { div { "hidden promoted={promoted} uncertain={uncertain} pending={pending}" } }
    } else {
        rsx! { DraftComposer { tab: Tab { id: ViewId(1), target, kind: TabKind::Transcript, preview: false, last_focused: 0 } } }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn first_message_survives_draft_composer_unmount() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex, mpsc},
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    SUBMISSION_TEST_URL
        .with(|url| *url.borrow_mut() = format!("http://{}", listener.local_addr().unwrap()));
    let requests = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let recorded = requests.clone();
    let (release_create, wait_create) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for index in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
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
                                .and_then(|s| s.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + len {
                        break (end + 4, len);
                    }
                }
            };
            let line = String::from_utf8_lossy(&data[..end])
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let body = String::from_utf8_lossy(&data[end..end + len]).into_owned();
            recorded.lock().unwrap().push((line.clone(), body));
            let (status, response) = match index {
                0 => {
                    assert!(line.starts_with("GET /sessions/1/agents "));
                    ("200 OK", serde_json::json!([]))
                }
                1 => {
                    assert!(line.starts_with("POST /sessions/1/agents "));
                    wait_create.recv_timeout(Duration::from_secs(5)).unwrap();
                    (
                        "200 OK",
                        serde_json::json!({"id":42,"definition_id":1,"session_id":1,"forked_from":null,"subagent_parent_id":null,"name":"root","hidden":false,"interactive":true,"state":"Idle","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0},"next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}),
                    )
                }
                _ => {
                    assert!(line.starts_with("POST /sessions/1/agents/42/requests "));
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    ("500 Internal Server Error", serde_json::json!({}))
                }
            };
            let response = response.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
    });
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(submission_fixture);
    let edits = dom.rebuild_to_vec();
    let textarea = edits
        .edits
        .iter()
        .find_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "keydown" => Some(*id),
            _ => None,
        })
        .unwrap();
    root_input(&mut dom, textarea, "first message");
    root_key(&mut dom, textarea, Key::Enter);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let _ = tokio::time::timeout(Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
        if requests.lock().unwrap().len() == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "agent creation did not start");
    }
    HIDE_SUBMISSION_COMPOSER.with(|slot| slot.get().unwrap().set(true));
    dom.render_immediate_to_vec();
    assert!(dioxus_ssr::render(&dom).contains("hidden promoted=false"));
    release_create.send(()).unwrap();
    loop {
        let _ = tokio::time::timeout(Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
        if dioxus_ssr::render(&dom).contains("promoted=true") && requests.lock().unwrap().len() == 3
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "request did not survive promotion: {:?}",
            requests.lock().unwrap()
        );
    }
    assert!(requests.lock().unwrap()[2].1.contains("first message"));
    release.send(()).unwrap();
    loop {
        let _ = tokio::time::timeout(Duration::from_millis(10), dom.wait_for_work()).await;
        dom.render_immediate_to_vec();
        if dioxus_ssr::render(&dom).contains("uncertain=true pending=false") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "uncertain result was not recorded"
        );
    }
    server.join().unwrap();
}

fn root_fixture() -> Element {
    let mut state = Ui::new(DesktopSettings::default());
    let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
        "config": {"config_id":{"config_group":"Global","name":"root"}, "selectable":true,
          "models":["Parent","Global",{"Fixed":{"model":"openrouter/vendor/model","effort":"Global"}}], "default":false},
        "status":{"missing_components":[]}
    })).unwrap();
    state.sessions.push(
        serde_json::from_value(serde_json::json!({
            "id":1,"forked_from":null,"name":"test","working_directory":"/tmp",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":"openrouter/favorite","effort":"Low"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    );
    let mut draft = crate::app::DraftRoot::default();
    draft.configs.push(status);
    draft.ready = true;
    state.draft_roots.insert(SessionId(1), draft.into());
    for (name, pinned) in [("Favorite", true), ("Other", false)] {
        state.models.push(
            serde_json::from_value(serde_json::json!({
                "model":format!("openrouter/{}", name.to_lowercase()), "name":name,
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
    rsx! { DraftComposer { tab:Tab { id:ViewId(1), target, kind: TabKind::Transcript, preview:false, last_focused:0 } } }
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

fn root_ui(dom: &VirtualDom) -> Signal<Ui> {
    dom.in_scope(ScopeId::APP, consume_context::<Signal<Ui>>)
}

fn root_draft(dom: &VirtualDom) -> String {
    root_ui(dom)
        .peek()
        .drafts
        .get(&TranscriptTarget {
            session_id: SessionId(1),
            agent_id: crate::app::DRAFT_ROOT,
        })
        .cloned()
        .unwrap_or_default()
}

fn root_input(dom: &mut VirtualDom, id: ElementId, value: &str) {
    root_input_without_render(dom, id, value);
    super::composer_tests::flush_dom(dom);
}

fn root_input_without_render(dom: &mut VirtualDom, id: ElementId, value: &str) {
    let data = SerializedFormData::new(value.into(), vec![]);
    dom.runtime().handle_event(
        "input",
        Event::new(
            Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
            true,
        ),
        id,
    );
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
    super::composer_tests::flush_dom(dom);
}

#[test]
fn root_key_uses_fresh_text_before_the_next_render() {
    let (mut dom, textarea) = mount_root();
    root_input_without_render(&mut dom, textarea, "/ag");
    root_key(&mut dom, textarea, Key::Enter);
    assert_eq!(root_draft(&dom), "/agent ");
    assert!(dioxus_ssr::render(&dom).contains("Choose a root agent"));
}

#[test]
fn root_key_rebuilds_configuration_candidates_from_current_metadata() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/agent ");
    root_ui(&dom)
        .write()
        .draft_roots
        .get_mut(&SessionId(1))
        .unwrap()
        .configs
        .clear();
    root_key(&mut dom, textarea, Key::Enter);
    assert_eq!(root_draft(&dom), "/agent ");
    assert!(dioxus_ssr::render(&dom).contains("No matching usable configurations"));
}

#[test]
fn root_completion_and_two_step_selection() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/ag");
    assert!(dioxus_ssr::render(&dom).contains("aria-label=\"Slash commands\""));
    root_key(&mut dom, textarea, Key::Tab);
    let html = dioxus_ssr::render(&dom);
    assert_eq!(root_draft(&dom), "/agent ");
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
    assert!(!html.contains("Model: openrouter/vendor/model"), "{html}");
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
    assert!(root_draft(&dom).is_empty(), "{html}");
}

#[test]
fn root_composer_is_blank_and_refresh_is_a_command() {
    let (mut dom, textarea) = mount_root();
    let html = dioxus_ssr::render(&dom);
    assert!(root_draft(&dom).is_empty(), "{html}");
    assert!(!html.contains("placeholder="), "{html}");
    assert!(html.contains("aria-label=\"Send first message\""), "{html}");
    assert!(!html.contains("Refresh configurations"), "{html}");
    root_input(&mut dom, textarea, "/ref");
    assert!(dioxus_ssr::render(&dom).contains("/refresh"));
    root_key(&mut dom, textarea, Key::Tab);
    assert_eq!(root_draft(&dom), "/refresh");
}

#[tokio::test(flavor = "current_thread")]
async fn root_refresh_executes_from_completion_and_is_not_sent_as_text() {
    let (mut dom, textarea) = mount_root();
    root_input(&mut dom, textarea, "/refresh");
    root_key(&mut dom, textarea, Key::Enter);
    let html = dioxus_ssr::render(&dom);
    assert!(root_draft(&dom).is_empty(), "{html}");
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
    assert_eq!(root_draft(&dom), "/model ");
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
