use super::*;

#[path = "submission_tests.rs"]
mod submission_tests;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone)]
struct Observer {
    ui: Signal<Ui>,
    renders: Rc<Cell<usize>>,
}

fn observed_root() -> Element {
    let observer = use_context::<Observer>();
    let _state = observer.ui.read();
    observer.renders.set(observer.renders.get() + 1);
    rsx! { div {} }
}

fn observed_ui() -> (VirtualDom, Signal<Ui>, Rc<Cell<usize>>) {
    let mut dom = VirtualDom::new(observed_root);
    let ui = dom.in_scope(ScopeId::ROOT, || {
        Signal::new(Ui::new(DesktopSettings::default()))
    });
    let renders = Rc::new(Cell::new(0));
    dom.insert_any_root_context(Box::new(Observer {
        ui,
        renders: renders.clone(),
    }));
    dom.rebuild_in_place();
    (dom, ui, renders)
}

fn target(agent: u64) -> TranscriptTarget {
    TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(agent),
    }
}

fn pending(agent: u64, revision: u64) -> PendingEvent {
    PendingEvent {
        id: "pending".into(),
        revision,
        session_id: SessionId(1),
        agent_id: AgentId(agent),
        kind: psa::api::events::EventType::Message(psa::api::events::Message::text(
            psa::api::events::MessageType::Agent,
            revision.to_string(),
        )),
    }
}

fn event(agent: u64, id: u64) -> EventEnvelope {
    serde_json::from_value(serde_json::json!({
        "session_id":1, "agent_id":agent, "position":id + 1000,
        "event": { "id":id, "source_agent_id":agent, "timestamp":"2026-01-01T00:00:00Z", "kind":pending(agent, id).kind }
    })).unwrap()
}

fn session_snapshot(id: u64, name: &str) -> Session {
    serde_json::from_value(serde_json::json!({
        "id":id, "forked_from":null, "name":name, "working_directory":"/tmp",
        "probe_id":"00000000-0000-0000-0000-000000000001",
        "model_config":{"model":"openrouter/model", "effort":"Medium"}, "busy":false,
        "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

fn agent_snapshot(id: u64) -> Agent {
    serde_json::from_value(serde_json::json!({
        "id":id, "definition_id":1, "session_id":1, "forked_from":null, "subagent_parent_id":null,
        "name":"root", "interactive":true, "hidden":false, "state":"Idle",
        "usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0},
        "next_event_id":1, "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z"
    })).unwrap()
}

fn query(agent: u64) -> ClientQueryRequest {
    ClientQueryRequest {
        id: ClientQueryId::new(),
        session_id: SessionId(1),
        agent_id: AgentId(agent),
        payload: ClientQuery::AskQuestion(AskQuestionRequest {
            questions: vec![ClientQuestion {
                question: "Continue?".into(),
                choices: Vec::new(),
            }],
        }),
    }
}

fn history(events: Vec<EventEnvelope>) -> EventsPage {
    EventsPage {
        events,
        pending: Vec::new(),
        queries: Vec::new(),
        next_after: 0,
        next_before: Some(1),
        has_older: true,
        head: 10,
        has_more: false,
    }
}

fn seed_transcripts(mut ui: Signal<Ui>, targets: &[TranscriptTarget]) {
    let mut state = ui.write();
    for target in targets {
        state.transcripts.insert(
            *target,
            Transcript::new(target.session_id, Some(target.agent_id)).into(),
        );
    }
}

fn summary_page(revision: u64, sessions: Vec<Session>) -> SessionsPage {
    SessionsPage {
        sessions,
        revision,
        next_before: None,
    }
}

#[test]
fn unchanged_summary_revisions_and_pages_do_not_publish() {
    let (mut dom, ui, renders) = observed_ui();
    let original = session_snapshot(1, "original");
    assert!(apply_summary_updates(ui, vec![(10, original.clone())]));
    dom.render_immediate_to_vec();
    let baseline = renders.get();
    let sessions = ui.peek().sessions.clone();
    let fresh = ui.peek().fresh_sessions.clone();

    assert!(!apply_summary_updates(ui, vec![(11, original.clone())]));
    assert!(!apply_summary_updates(
        ui,
        vec![(9, session_snapshot(1, "stale"))]
    ));
    // A batch which changes a value and restores it is also a visible no-op.
    assert!(!apply_summary_updates(
        ui,
        vec![
            (12, session_snapshot(1, "temporary")),
            (13, original.clone())
        ]
    ));
    merge_page(ui, summary_page(10, vec![session_snapshot(1, "late page")]));
    merge_page(ui, summary_page(13, vec![original.clone()]));
    merge_page(
        ui,
        summary_page(
            13,
            vec![session_snapshot(1, "temporary page item"), original.clone()],
        ),
    );
    dom.render_immediate_to_vec();
    assert_eq!(
        renders.get(),
        baseline,
        "bookkeeping must not acquire a reactive write"
    );
    assert_eq!(ui.peek().sessions, sessions);
    assert_eq!(ui.peek().fresh_sessions, fresh);
    assert_eq!(ui.peek().network.borrow().revisions[&SessionId(1)], 13);

    let mut busy = original;
    busy.busy = true; // Activity-only updates do not change updated_at.
    assert!(apply_summary_updates(ui, vec![(14, busy)]));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), baseline + 1);
    assert!(ui.peek().sessions[0].busy);
}

#[test]
fn summary_batches_preserve_first_insertion_order_and_last_accepted_revision() {
    let (_dom, ui, _renders) = observed_ui();
    assert!(apply_summary_updates(
        ui,
        vec![
            (5, session_snapshot(2, "first")),
            (6, session_snapshot(1, "second")),
            (4, session_snapshot(2, "stale")),
            (5, session_snapshot(2, "equal revision wins")),
        ]
    ));
    assert_eq!(
        ui.peek()
            .sessions
            .iter()
            .map(|s| s.id.0)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(ui.peek().sessions[0].name, "equal revision wins");
    merge_page(ui, summary_page(4, vec![session_snapshot(2, "late page")]));
    assert_eq!(ui.peek().sessions[0].name, "equal revision wins");
    merge_page(ui, summary_page(7, vec![session_snapshot(2, "new page")]));
    assert_eq!(ui.peek().sessions[0].name, "new page");
}

#[test]
fn probe_comparison_includes_metadata_and_error_recovery_without_stale_writes() {
    let mut state = Ui::new(DesktopSettings::default());
    state.network.borrow_mut().probe_request = 2;
    let probe: Probe = serde_json::from_value(serde_json::json!({
        "id":"00000000-0000-0000-0000-000000000001", "hostname":"host", "os":"linux", "architecture":"x86_64",
        "label":null, "data_dir":"/tmp", "home_dir":"/home/test", "host":false, "is_active":true,
        "created_at":"2026-01-01T00:00:00Z", "last_connected_at":"2026-01-01T00:00:00Z"
    })).unwrap();
    apply_probe_refresh(&mut state, 0, 2, Ok(vec![probe.clone()]));
    assert!(!probe_refresh_changes(
        &state,
        0,
        2,
        &Ok(vec![probe.clone()])
    ));
    assert!(!probe_refresh_changes(&state, 0, 1, &Err("stale".into())));
    assert!(!probe_refresh_changes(
        &state,
        1,
        2,
        &Err("other profile".into())
    ));
    let mut changed = probe.clone();
    changed.metadata.home_dir = Some("/new-home".into());
    assert!(probe_refresh_changes(&state, 0, 2, &Ok(vec![changed])));
    state.probe_error = Some("offline".into());
    assert!(!probe_refresh_changes(&state, 0, 2, &Err("offline".into())));
    assert!(probe_refresh_changes(
        &state,
        0,
        2,
        &Ok(vec![probe.clone()])
    ));
    apply_probe_refresh(&mut state, 0, 2, Ok(vec![probe]));
    assert!(state.probe_error.is_none());
}

#[test]
fn stale_and_unchanged_probe_responses_do_not_invalidate_ui() {
    let (mut dom, ui, renders) = observed_ui();
    assert!(!publish_probe_refresh(ui, 0, 0, Ok(Vec::new())));
    assert!(!publish_probe_refresh(
        ui,
        1,
        0,
        Err("stale profile".into())
    ));
    assert!(!publish_probe_refresh(
        ui,
        0,
        1,
        Err("stale request".into())
    ));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), 1);
    assert!(publish_probe_refresh(ui, 0, 0, Err("offline".into())));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), 2);
    assert!(!publish_probe_refresh(ui, 0, 0, Err("offline".into())));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), 2);
    assert!(publish_probe_refresh(ui, 0, 0, Ok(Vec::new())));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), 3);
    assert!(ui.peek().probe_error.is_none());
}

#[test]
fn unrelated_payloads_never_publish_or_clone_other_transcripts() {
    let (mut dom, ui, renders) = observed_ui();
    let targets = [target(2), target(3)];
    seed_transcripts(ui, &targets);
    dom.render_immediate_to_vec();
    let baseline = renders.get();
    let other = ui.peek().transcripts[&target(3)].clone();
    let mut batch = StreamBatch::default();
    batch.push(StreamMessage::Upsert {
        pending: pending(99, 1),
    });
    assert!(!publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), baseline);

    batch.push(StreamMessage::Upsert {
        pending: pending(2, 1),
    });
    assert!(publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    assert_eq!(ui.peek().transcripts[&target(3)], other);
    assert_eq!(
        ui.peek().transcripts[&target(2)].pending[&psa::api::PendingEventId::from("pending")]
            .revision,
        1
    );
}

#[test]
fn stale_stream_upserts_and_identical_metadata_do_not_publish() {
    let (mut dom, ui, renders) = observed_ui();
    let targets = [target(2)];
    seed_transcripts(ui, &targets);
    let question = query(2);
    let messages = vec![
        StreamMessage::Upsert {
            pending: pending(2, 7),
        },
        StreamMessage::AgentUpdated {
            agent: agent_snapshot(2),
        },
        StreamMessage::SessionUpdated {
            session: session_snapshot(1, "same"),
        },
        StreamMessage::ClientQueryRequested { query: question },
    ];
    let mut batch = StreamBatch::default();
    for message in &messages {
        batch.push(message.clone());
    }
    assert!(publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    dom.render_immediate_to_vec();
    let baseline = renders.get();
    let transcript = ui.peek().transcripts[&target(2)].clone();
    let agents = ui.peek().agents.clone();
    for message in messages {
        batch.push(message);
    }
    batch.push(StreamMessage::Upsert {
        pending: pending(2, 6),
    });
    assert!(!publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    dom.render_immediate_to_vec();
    assert_eq!(renders.get(), baseline);
    assert_eq!(ui.peek().transcripts[&target(2)], transcript);
    assert_eq!(ui.peek().agents, agents);
}

fn assert_same_projection(actual: &Transcript, expected: &Transcript) {
    fn wire<T: serde::Serialize>(value: T) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }
    assert_eq!(
        wire(
            actual
                .committed
                .values()
                .map(|value| &**value)
                .collect::<Vec<_>>()
        ),
        wire(
            expected
                .committed
                .values()
                .map(|value| &**value)
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(
        wire(
            actual
                .pending
                .values()
                .map(|value| &**value)
                .collect::<Vec<_>>()
        ),
        wire(
            expected
                .pending
                .values()
                .map(|value| &**value)
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(actual.queries, expected.queries);
    assert_eq!(
        serde_json::to_value(&actual.agents).unwrap(),
        serde_json::to_value(&expected.agents).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.session).unwrap(),
        serde_json::to_value(&expected.session).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.context_state).unwrap(),
        serde_json::to_value(&expected.context_state).unwrap()
    );
    assert_eq!(actual.request_failures, expected.request_failures);
    assert_eq!(
        (
            actual.cursor,
            actual.needs_resync,
            actual.next_before,
            actual.has_older
        ),
        (
            expected.cursor,
            expected.needs_resync,
            expected.next_before,
            expected.has_older
        )
    );
}

#[test]
fn compacted_batches_are_equivalent_to_ordered_reducer_application() {
    let (_dom, ui, _renders) = observed_ui();
    let targets = [target(2), target(3)];
    seed_transcripts(ui, &targets);
    let question = query(2);
    let mut equal_revision = pending(2, 3);
    equal_revision.kind = pending(2, 999).kind;
    let messages = vec![
        StreamMessage::Snapshot {
            cursor: 9000,
            pending: vec![pending(2, 1), pending(3, 1), pending(99, 1)],
            queries: vec![question.clone(), query(99)],
        },
        StreamMessage::Upsert {
            pending: pending(2, 3),
        },
        StreamMessage::Upsert {
            pending: pending(2, 1),
        },
        StreamMessage::Upsert {
            pending: equal_revision,
        },
        StreamMessage::Removed {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            id: "pending".into(),
        },
        StreamMessage::Upsert {
            pending: pending(2, 10),
        },
        StreamMessage::Snapshot {
            cursor: 10000,
            pending: vec![pending(2, 1)],
            queries: vec![],
        },
        StreamMessage::Upsert {
            pending: pending(2, 2),
        },
        StreamMessage::Committed {
            event: event(2, 5),
            pending_id: Some("pending".into()),
        },
        StreamMessage::Committed {
            event: event(2, 5),
            pending_id: None,
        },
        StreamMessage::Upsert {
            pending: pending(2, 100),
        },
        StreamMessage::ClientQueryRequested {
            query: question.clone(),
        },
        StreamMessage::AgentUpdated {
            agent: agent_snapshot(2),
        },
        StreamMessage::SessionUpdated {
            session: session_snapshot(1, "updated"),
        },
        StreamMessage::RequestFailed {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            request_id: "request".into(),
            message: "failed".into(),
        },
        StreamMessage::ClientQueryRemoved {
            session_id: SessionId(1),
            agent_id: AgentId(2),
            query_id: question.id,
        },
    ];
    let mut expected =
        targets.map(|target| Transcript::new(target.session_id, Some(target.agent_id)));
    let mut batch = StreamBatch::default();
    for message in &messages {
        for transcript in &mut expected {
            let mut message = message.clone();
            if let StreamMessage::Snapshot { cursor, .. } = &mut message {
                *cursor = transcript.cursor;
            }
            transcript.apply(message);
        }
        batch.push(message.clone());
    }
    assert!(batch.messages.len() < messages.len());
    assert!(publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    for (target, expected) in targets.iter().zip(&expected) {
        assert_same_projection(&ui.peek().transcripts[target], expected);
    }
    assert_eq!(
        ui.peek().transcripts[&target(2)].cursor,
        5,
        "session snapshot cursors must not leak into agent cursors"
    );
    assert_eq!(ui.peek().transcripts[&target(3)].cursor, 0);
    assert_eq!(ui.peek().error.as_deref(), Some("Request request: failed"));
}

#[test]
fn frame_flush_preserves_history_loaded_after_enqueue_and_discards_old_generation() {
    let (_dom, mut ui, _renders) = observed_ui();
    let targets = [target(2)];
    seed_transcripts(ui, &targets);
    let mut batch = StreamBatch::default();
    batch.push(StreamMessage::Committed {
        event: event(2, 5),
        pending_id: None,
    });
    ui.write()
        .transcripts
        .get_mut(&target(2))
        .unwrap()
        .merge_history(history(vec![event(2, 1)]));
    assert!(publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    assert_eq!(ui.peek().transcripts[&target(2)].committed.len(), 2);
    assert_eq!(ui.peek().transcripts[&target(2)].cursor, 5);
    assert_eq!(ui.peek().transcripts[&target(2)].next_before, Some(1));

    batch.push(StreamMessage::Upsert {
        pending: pending(2, 99),
    });
    ui.write().generation += 1;
    assert!(!publish_session_batch(
        ui,
        0,
        SessionId(1),
        &targets,
        &mut batch
    ));
    assert!(ui.peek().transcripts[&target(2)].pending.is_empty());
    assert_eq!(batch.received, 0);
    assert!(batch.messages.is_empty());
    let mut summaries = vec![(100, session_snapshot(1, "stale profile"))];
    assert!(!publish_summary_batch(ui, 0, &mut summaries));
    assert!(ui.peek().sessions.is_empty());
}

#[test]
fn batch_bound_counts_received_messages_even_when_compaction_reduces_storage() {
    let mut batch = StreamBatch::default();
    for revision in 0..MAX_STREAM_BATCH {
        batch.push(StreamMessage::Upsert {
            pending: pending(2, revision as u64),
        });
    }
    assert!(batch.is_full());
    assert_eq!(batch.received, MAX_STREAM_BATCH);
    assert_eq!(batch.messages.len(), 1);
    let StreamMessage::Upsert { pending } = &batch.messages[0] else {
        panic!("expected upsert")
    };
    assert_eq!(pending.revision, (MAX_STREAM_BATCH - 1) as u64);
    assert!(!message_is_immediate(&batch.messages[0]));
    assert!(message_is_immediate(&StreamMessage::ClientQueryRequested {
        query: query(2)
    }));
    assert!(message_is_immediate(&StreamMessage::Removed {
        session_id: SessionId(1),
        agent_id: AgentId(2),
        id: "pending".into()
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn full_stream_batch_applies_backpressure_until_frame_and_query_is_immediate() {
    let (dom, ui, _renders) = observed_ui();
    let targets = [target(2)];
    seed_transcripts(ui, &targets);
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let question = query(2);
    let mut items = (0..MAX_STREAM_BATCH)
        .map(|revision| {
            Ok(StreamEvent {
                id: String::new(),
                message: StreamMessage::Upsert {
                    pending: pending(2, revision as u64),
                },
            })
        })
        .collect::<Vec<_>>();
    items.push(Ok(StreamEvent {
        id: String::new(),
        message: StreamMessage::ClientQueryRequested {
            query: question.clone(),
        },
    }));
    let mut stream: EventStream = Box::pin(
        futures_util::stream::iter(items)
            .inspect(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            })
            .chain(futures_util::stream::pending()),
    );
    let mut run = Box::pin(async move {
        let mut consume = Box::pin(consume_session_stream(
            ui,
            0,
            SessionId(1),
            &targets,
            &mut stream,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(2), consume.as_mut())
                .await
                .is_err()
        );
        assert_eq!(reads.load(Ordering::Relaxed), MAX_STREAM_BATCH);
        assert!(ui.peek().transcripts[&target(2)].pending.is_empty());
        let observe = async {
            loop {
                if ui.peek().transcripts[&target(2)]
                    .queries
                    .contains_key(&question.id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::select! {
            result = consume.as_mut() => panic!("unexpected stream termination: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(1), observe) => result.unwrap(),
        }
        assert_eq!(reads.load(Ordering::Relaxed), MAX_STREAM_BATCH + 1);
        assert_eq!(
            ui.peek().transcripts[&target(2)].pending[&psa::api::PendingEventId::from("pending")]
                .revision,
            (MAX_STREAM_BATCH - 1) as u64
        );
    });
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stream_error_flushes_ordered_updates_but_profile_change_discards_them() {
    let (dom, mut ui, _renders) = observed_ui();
    let targets = [target(2)];
    seed_transcripts(ui, &targets);
    let mut run = Box::pin(async move {
        let mut stream: EventStream = Box::pin(futures_util::stream::iter(vec![
            Ok(StreamEvent {
                id: String::new(),
                message: StreamMessage::Upsert {
                    pending: pending(2, 1),
                },
            }),
            Err(ClientError::Stream("connection lost".into())),
        ]));
        assert!(
            consume_session_stream(ui, 0, SessionId(1), &targets, &mut stream)
                .await
                .is_err()
        );
        assert_eq!(
            ui.peek().transcripts[&target(2)].pending[&psa::api::PendingEventId::from("pending")]
                .revision,
            1
        );
        ui.write()
            .transcripts
            .get_mut(&target(2))
            .unwrap()
            .apply(StreamMessage::Snapshot {
                cursor: 0,
                pending: Vec::new(),
                queries: Vec::new(),
            });
        let mut stream: EventStream = Box::pin(
            futures_util::stream::once(async {
                Ok(StreamEvent {
                    id: String::new(),
                    message: StreamMessage::Upsert {
                        pending: pending(2, 2),
                    },
                })
            })
            .chain(futures_util::stream::pending()),
        );
        let mut consume = Box::pin(consume_session_stream(
            ui,
            0,
            SessionId(1),
            &targets,
            &mut stream,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(2), consume.as_mut())
                .await
                .is_err()
        );
        ui.write().generation += 1;
        tokio::time::timeout(Duration::from_secs(1), consume)
            .await
            .unwrap()
            .unwrap();
        assert!(ui.peek().transcripts[&target(2)].pending.is_empty());
    });
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
}

#[tokio::test(flavor = "current_thread")]
async fn authentication_failures_are_not_retried_and_stale_failures_do_not_publish() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..3 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = socket.read(&mut buffer).unwrap();
                assert_ne!(length, 0);
                request.extend_from_slice(&buffer[..length]);
            }
            requests.push(String::from_utf8(request).unwrap());
            if index == 2 {
                std::thread::sleep(Duration::from_millis(40));
            }
            let body = r#"{"code":"forbidden","message":"rejected"}"#;
            write!(socket, "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    let (dom, mut ui, _renders) = observed_ui();
    ui.write().settings.active_profile_mut().connection.api_url = format!("http://{address}");
    let mut run = Box::pin(async move {
        tokio::time::timeout(Duration::from_millis(200), summaries(ui, 0))
            .await
            .unwrap();
        assert!(ui.peek().connection.starts_with("Authentication failed"));
        tokio::time::timeout(
            Duration::from_millis(200),
            session(ui, SessionId(1), vec![target(2)]),
        )
        .await
        .unwrap();
        assert!(ui.peek().transcript_status[&target(2)].starts_with("Authentication failed"));
        ui.write()
            .change_connection(format!("http://{address}"), String::new());
        let change_profile = async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            ui.write()
                .change_connection("http://127.0.0.1:1".into(), String::new());
        };
        futures_util::join!(session(ui, SessionId(1), vec![target(2)]), change_profile);
        assert!(ui.peek().transcript_status.is_empty());
        assert!(ui.peek().error.is_none());
    });
    std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
    assert_eq!(server.join().unwrap().len(), 3);
}
