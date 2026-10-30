use crate::*;
use psa_api::*;
use serde_json::json;
fn session(id: u64, path: &str) -> Session {
    serde_json::from_value(json!({"id":id,"parent_id":null,"name":format!("session{id}"),"working_directory":path,"probe_id":"00000000-0000-0000-0000-000000000001","model_config":{"model":{"provider":"Openrouter","api_name":"test/model"},"effort":"Medium"},"busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"})).unwrap()
}
fn agent(id: u64, parent: Option<u64>, hidden: bool, interactive: bool) -> Agent {
    serde_json::from_value(json!({"id":id,"definition_id":1,"session_id":1,"parent_id":99,"subagent_parent_id":parent,"name":format!("a{id}"),"hidden":hidden,"interactive":interactive,"state":"Idle","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0},"next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"})).unwrap()
}
#[test]
fn view_ids_are_not_reused_and_drafts_are_not_persisted() {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(2),
    };
    let mut w = Workspace::default();
    let first = w.open(target, false);
    let mut drafts = Drafts::default();
    drafts.set(target, "unfinished".into());
    w.promote(first);
    assert!(!w.active_tab().unwrap().preview);
    let encoded = serde_json::to_string(&w).unwrap();
    assert!(!encoded.contains("unfinished"));
    w.close(first);
    assert_ne!(w.open(target, false), first);
    assert_eq!(drafts.get(target), "unfinished");
}
#[test]
fn follow_is_view_local_and_tail_is_bounded() {
    let mut first = FollowState::default();
    let mut second = FollowState::default();
    second.set_near_bottom(false);
    assert!(first.record_activity());
    assert!(!second.record_activity());
    assert_eq!(second.unseen_updates, 1);
    second.set_near_bottom(true);
    assert_eq!(second.unseen_updates, 0);
    let mut transcript = Transcript::new(SessionId(1), None);
    for i in 0..10 {
        transcript.apply(StreamMessage::Upsert {
            pending: PendingEvent {
                id: format!("{i:02}"),
                revision: 1,
                session_id: SessionId(1),
                agent_id: AgentId(2),
                kind: psa_api::events::EventType::NewContext,
            },
        });
    }
    assert_eq!(transcript.recent_rows(3).len(), 3);
    assert!(transcript.recent_rows(0).is_empty());
}
#[test]
fn grouping_uses_exact_path_and_stable_recency_not_busy() {
    let mut index = SessionIndex::default();
    index.upsert(session(1, "/a/b"));
    index.upsert(session(2, "/a/./b"));
    index.upsert(session(3, "/a/b"));
    let mut older = session(4, "/older");
    older.updated_at = time::OffsetDateTime::UNIX_EPOCH;
    older.busy = true;
    index.upsert(older);
    let groups = index.groups(None);
    assert_eq!(groups.len(), 3);
    assert_eq!(
        groups[0]
            .sessions
            .iter()
            .map(|s| s.id.0)
            .collect::<Vec<_>>(),
        vec![3, 1]
    );
    assert_eq!(groups[1].working_directory.as_os_str(), "/a/./b");
    assert!(groups[2].busy);
    assert!(!groups[0].complete);
}
#[test]
fn page_race_does_not_overwrite_busy_only_updates() {
    let mut index = SessionIndex::default();
    let generation = index.begin_bootstrap();
    index.apply_global(generation, GlobalStreamMessage::Ready { revision: 1 });
    let mut live = session(1, "/a");
    live.busy = true;
    index.apply_global(
        generation,
        GlobalStreamMessage::SessionUpdated {
            revision: 3,
            session: live,
        },
    );
    assert!(index.merge_page(
        generation,
        SessionsPage {
            sessions: vec![session(1, "/a")],
            next_before: None,
            revision: 2
        }
    ));
    assert!(index.sessions[&SessionId(1)].busy);
    let next = index.begin_bootstrap();
    assert_ne!(next, generation);
    assert!(!index.merge_page(
        generation,
        SessionsPage {
            sessions: vec![session(2, "/old")],
            next_before: None,
            revision: 99
        }
    ));
    assert_eq!(index.sessions.len(), 1);
    index.apply_global(next, GlobalStreamMessage::ResyncRequired);
    assert!(index.needs_resync);
}
#[test]
fn metadata_list_race_retains_live_agent_updates() {
    let mut index = AgentIndex::default();
    let generation = index.begin_bootstrap();
    let mut live = agent(1, None, false, true);
    live.state = AgentState::Running;
    index.apply_live(generation, live);
    assert!(index.finish_bootstrap(generation, vec![agent(1, None, false, true)]));
    assert_eq!(index.agents[&AgentId(1)].state, AgentState::Running);
    index.begin_bootstrap();
    assert!(!index.finish_bootstrap(generation, vec![]));
    assert_eq!(index.agents.len(), 1);
}
#[test]
fn ancestry_ignores_fork_lineage_and_preserves_hidden_context() {
    let agents = vec![
        agent(1, None, true, false),
        agent(2, Some(1), false, true),
        agent(3, None, false, false),
    ];
    let rows = agent_tree(&agents, false);
    assert_eq!(
        rows.iter()
            .map(|r| (r.agent.id.0, r.depth, r.context_only))
            .collect::<Vec<_>>(),
        vec![(1, 0, true), (2, 1, false), (3, 0, false)]
    );
    assert_eq!(select_session_agent(&agents, None), Some(AgentId(3)));
    assert_eq!(
        select_session_agent(&agents, Some(AgentId(2))),
        Some(AgentId(2))
    );
    assert_eq!(
        select_session_agent(&agents, Some(AgentId(1))),
        Some(AgentId(3))
    );
}
#[test]
fn cycles_orphans_and_no_visible_roots_are_safe() {
    let agents = vec![
        agent(1, Some(2), false, true),
        agent(2, Some(1), true, false),
        agent(3, Some(999), false, true),
    ];
    assert_eq!(agent_tree(&agents, false).len(), 3);
    assert_eq!(select_session_agent(&agents, None), None);
}
#[test]
fn query_removal_and_history_do_not_resurrect_answers() {
    let mut t = Transcript::new(SessionId(1), Some(AgentId(2)));
    let query = ClientQueryRequest {
        id: ClientQueryId::new(),
        session_id: SessionId(1),
        agent_id: AgentId(2),
        payload: ClientQuery::AskQuestion(AskQuestionRequest {
            questions: vec![ClientQuestion {
                question: "Which?".into(),
                choices: vec![],
            }],
        }),
    };
    t.apply(StreamMessage::ClientQueryRequested {
        query: query.clone(),
    });
    assert_eq!(t.queries.len(), 1);
    t.apply(StreamMessage::ClientQueryRemoved {
        session_id: SessionId(1),
        agent_id: AgentId(2),
        query_id: query.id,
    });
    t.merge_history(EventsPage {
        events: vec![],
        pending: vec![],
        queries: vec![query],
        next_after: 0,
        next_before: None,
        has_older: false,
        head: 0,
        has_more: false,
    });
    assert!(t.queries.is_empty());
}
#[test]
fn tool_results_correlate_by_agent_not_name_or_adjacency() {
    use psa_api::events::*;
    let mut t = Transcript::new(SessionId(1), None);
    for (position, agent_id, kind) in [
        (
            1,
            1,
            json!({"ToolResult":{"call_id":"same","content":[],"error":true,"metadata":{"provider_metadata":{}}}}),
        ),
        (
            2,
            2,
            json!({"ToolCall":{"call_id":"same","tool_name":"bash","arguments":"{}","metadata":{"provider_metadata":{}}}}),
        ),
        (
            3,
            1,
            json!({"ToolCall":{"call_id":"same","tool_name":"read","arguments":"{bad","metadata":{"provider_metadata":{}}}}),
        ),
    ] {
        t.apply(StreamMessage::Committed {
            event: EventEnvelope {
                session_id: SessionId(1),
                agent_id: AgentId(agent_id),
                position,
                event: Event {
                    id: EventId(position),
                    source_agent_id: AgentId(agent_id),
                    timestamp: time::OffsetDateTime::UNIX_EPOCH,
                    kind: serde_json::from_value(kind).unwrap(),
                },
            },
            pending_id: None,
        });
    }
    let tools = t.tool_exchanges();
    assert_eq!(tools.len(), 2);
    assert!(tools[0].result.as_ref().unwrap().error);
    assert!(!tools[0].running);
    assert!(tools[1].running);
    let visible_ids = std::collections::HashSet::from(["same".to_owned()]);
    assert_eq!(t.tool_exchanges_for(&visible_ids).len(), 2);
    assert!(
        t.tool_exchanges_for(&std::collections::HashSet::new())
            .is_empty()
    );
}
