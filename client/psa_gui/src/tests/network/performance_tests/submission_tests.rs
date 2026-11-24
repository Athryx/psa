use super::*;

#[tokio::test(flavor = "current_thread")]
async fn action_time_regular_and_root_submissions_send_captured_text_and_keep_newer_edits() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let (body_start, body_length) = loop {
                let length = socket.read(&mut buffer).unwrap();
                assert_ne!(length, 0);
                request.extend_from_slice(&buffer[..length]);
                let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break (end + 4, length);
                }
            };
            let line = String::from_utf8_lossy(&request[..body_start])
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let body =
                String::from_utf8(request[body_start..body_start + body_length].to_vec()).unwrap();
            let response = if line.starts_with("GET /sessions/1/agents ") {
                serde_json::json!([])
            } else if line.starts_with("POST /sessions/1/agents ") {
                serde_json::to_value(agent_snapshot(42)).unwrap()
            } else if line.starts_with("POST /sessions/1/agents/3/requests ")
                || line.starts_with("POST /sessions/1/agents/42/requests ")
            {
                serde_json::json!({"request_id":"accepted"})
            } else {
                panic!("unexpected request: {line}");
            };
            requests.push((line, body));
            let body = response.to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    let (dom, mut ui, _renders) = observed_ui();
    ui.write().settings.active_profile_mut().connection.api_url = format!("http://{address}");
    let mut run = Box::pin(async move {
        ui.write()
            .drafts
            .insert(target(3), "regular original".into());
        let regular = submit(ui, target(3), false);
        ui.write()
            .drafts
            .insert(target(3), "regular newer edit".into());
        regular.await;
        assert_eq!(
            ui.peek().drafts.get(&target(3)).map(String::as_str),
            Some("regular newer edit")
        );
        assert!(ui.peek().uncertain_submissions.is_empty());

        let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
            "config": {"config_id":{"config_group":"Global","name":"captured-root"}, "selectable":true,"models":["Global"],"default":true},
            "status":{"missing_components":[]}
        })).unwrap();
        let mut root = crate::app::DraftRoot::default();
        root.configs.push(status.clone());
        root.select_config(status.config.config_id);
        ui.write().draft_roots.insert(SessionId(1), root.into());
        ui.write().drafts.insert(target(0), "root original".into());
        let root = submit_draft_root(ui, SessionId(1));
        assert!(
            ui.peek().draft_roots[&SessionId(1)].pending,
            "deduplication begins at the action, not its first poll"
        );
        let duplicate = submit_draft_root(ui, SessionId(1));
        duplicate.await;
        {
            let mut state = ui.write();
            state.drafts.insert(target(0), "root newer edit".into());
            let draft = state.draft_roots.get_mut(&SessionId(1)).unwrap();
            draft.selected = None;
            draft.model = None;
        }
        root.await;
        assert_eq!(
            ui.peek().drafts.get(&target(42)).map(String::as_str),
            Some("root newer edit")
        );
        assert!(!ui.peek().drafts.contains_key(&target(0)));
        assert!(ui.peek().draft_roots.is_empty());
        assert!(ui.peek().uncertain_submissions.is_empty());
        assert!(ui.peek().error.is_none());
    });
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].1.contains("regular original"));
    assert!(!requests[0].1.contains("regular newer edit"));
    let create: serde_json::Value = serde_json::from_str(&requests[2].1).unwrap();
    assert_eq!(create["config_id"]["name"], "captured-root");
    assert_eq!(create["model"], "Global");
    assert!(requests[3].1.contains("root original"));
    assert!(!requests[3].1.contains("root newer edit"));
}

#[test]
fn promotion_conflicts_preserve_both_drafts_and_root_view_without_resubmission() {
    for destination_text in [None, Some("existing draft")] {
        let mut state = Ui::new(DesktopSettings::default());
        let from = target(0);
        let to = target(42);
        state.open(from, true);
        let tab = state.workspace.active_tab().unwrap().id;
        state.drafts.insert(from, "first message".into());
        if let Some(text) = destination_text {
            state.drafts.insert(to, text.into());
        } else {
            // An initialized editor may contain DOM edits not yet delivered to Rust.
            state.drafts.ensure_browser_key(to);
        }
        let source_key = state.drafts.browser_key(&from).unwrap().to_owned();
        let destination_key = state.drafts.browser_key(&to).unwrap().to_owned();
        state.draft_roots.entry(SessionId(1)).or_default().pending = true;
        assert!(promote_draft_root(&mut state, SessionId(1), agent_snapshot(42)).is_none());
        assert_eq!(state.workspace.active_tab().unwrap().id, tab);
        assert_eq!(state.workspace.active_tab().unwrap().target, from);
        assert_eq!(
            state.drafts.get(&from).map(String::as_str),
            Some("first message")
        );
        assert_eq!(state.drafts.get(&to).map(String::as_str), destination_text);
        assert_eq!(state.drafts.browser_key(&from), Some(source_key.as_str()));
        assert_eq!(
            state.drafts.browser_key(&to),
            Some(destination_key.as_str())
        );
        let root = &state.draft_roots[&SessionId(1)];
        assert!(!root.pending);
        assert!(
            root.uncertain,
            "a conflict must prevent another create/send attempt"
        );
        assert!(state.pending_submissions.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("Copy or merge"));
    }
}

#[test]
fn profile_changes_between_capture_and_first_poll_cancel_the_captured_action() {
    let (dom, mut ui, _renders) = observed_ui();
    ui.write().drafts.insert(target(2), "old profile".into());
    let mut submission = Box::pin(submit(ui, target(2), false));
    ui.write()
        .change_connection("http://127.0.0.1:1".into(), String::new());
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    assert!(
        dom.in_scope(ScopeId::ROOT, || submission.as_mut().poll(&mut context))
            .is_ready()
    );
    assert!(ui.peek().pending_submissions.is_empty());
    assert!(ui.peek().uncertain_submissions.is_empty());
    assert!(ui.peek().notice.is_none());
    assert!(ui.peek().error.is_none());
}
