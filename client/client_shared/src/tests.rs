use crate::*;
use psa::api::*;
use serde_json::json;
fn session(id: u64, path: &str) -> Session {
    serde_json::from_value(json!({"id":id,"parent_id":null,"name":format!("session{id}"),"working_directory":path,"probe_id":"00000000-0000-0000-0000-000000000001","model_config":{"model":"openrouter/test/model","effort":"Medium"},"busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"})).unwrap()
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
                kind: psa::api::events::EventType::NewContext,
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
    let groups = index.groups(&Default::default());
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
fn grouping_preserves_probe_identity_all_path_spellings_and_recency() {
    let mut index = SessionIndex::default();
    index.fully_loaded = true;
    let other_probe = ProbeId(uuid::Uuid::from_u128(2));
    for (id, path) in [
        (1, "/work/tree"),
        (2, "/work/./tree"),
        (3, "/work//tree"),
        (4, "/work/tree/"),
        (5, "/work/tree"),
        (6, "/work/tree"),
    ] {
        let mut session = session(id, path);
        session.busy = id == 1;
        if id == 5 {
            session.probe_id = other_probe;
        }
        if id == 6 {
            session.updated_at = time::OffsetDateTime::UNIX_EPOCH;
        }
        index.upsert(session);
    }
    let groups = index.groups(&Default::default());
    assert_eq!(groups.len(), 5);
    assert_eq!(
        groups
            .iter()
            .map(|group| group.sessions.iter().map(|s| s.id.0).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        vec![vec![5], vec![4], vec![3], vec![2], vec![1, 6]]
    );
    assert_eq!(
        groups
            .iter()
            .map(|g| g.working_directory.as_os_str())
            .collect::<Vec<_>>(),
        vec![
            "/work/tree",
            "/work/tree/",
            "/work//tree",
            "/work/./tree",
            "/work/tree"
        ]
    );
    assert!(groups.iter().all(|g| g.complete));
    assert!(groups[4].busy);
    assert!(!groups[0].busy);
    assert_eq!(
        index.groups(&probe_filter(other_probe))[0].sessions[0].id,
        SessionId(5)
    );
    let first_probe = index.sessions[&SessionId(1)].probe_id;
    assert_eq!(index.groups(&probe_filter(first_probe)).len(), 4);
}

#[cfg(unix)]
#[test]
fn grouping_hashes_original_non_utf8_os_strings_without_loss() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let mut index = SessionIndex::default();
    for (id, bytes) in [(1, b"/work/\xff".to_vec()), (2, b"/work/\xfe".to_vec())] {
        let mut session = session(id, "/unused");
        session.working_directory = OsString::from_vec(bytes).into();
        index.upsert(session);
    }
    let mut duplicate = index.sessions[&SessionId(1)].clone();
    duplicate.id = SessionId(3);
    index.upsert(duplicate);
    let groups = index.groups(&Default::default());
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].sessions.len(), 2);
    assert_ne!(
        groups[0].working_directory.as_os_str(),
        groups[1].working_directory.as_os_str()
    );
}

#[test]
fn grouping_many_distinct_folders_keeps_descending_id_ties() {
    let template = session(0, "/unused");
    let mut index = SessionIndex::default();
    for id in 1..=16_384 {
        let mut session = template.clone();
        session.id = SessionId(id);
        session.working_directory = format!("/work/{id}").into();
        index.upsert(session);
    }
    let groups = index.groups(&Default::default());
    assert_eq!(groups.len(), 16_384);
    for (offset, group) in groups.iter().enumerate() {
        assert_eq!(group.sessions.len(), 1);
        assert_eq!(group.sessions[0].id.0, 16_384 - offset as u64);
    }
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
/// The original straightforward traversal is a small-input ordering/visibility oracle.
fn reference_agent_tree(agents: &[Agent], show_hidden: bool) -> Vec<(u64, usize, bool)> {
    use std::collections::{HashMap, HashSet};

    let by_id: HashMap<_, _> = agents.iter().map(|a| (a.id, a)).collect();
    let mut included: HashSet<_> = agents
        .iter()
        .filter(|a| show_hidden || !a.hidden)
        .map(|a| a.id)
        .collect();
    for a in agents.iter().filter(|a| show_hidden || !a.hidden) {
        let mut current = a.subagent_parent_id;
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            let Some(parent) = by_id.get(&id) else {
                break;
            };
            included.insert(id);
            current = parent.subagent_parent_id;
        }
    }
    let mut sorted: Vec<_> = agents.iter().filter(|a| included.contains(&a.id)).collect();
    sorted.sort_by_key(|a| a.id.0);
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    fn visit(
        a: &Agent,
        depth: usize,
        sorted: &[&Agent],
        visited: &mut HashSet<AgentId>,
        out: &mut Vec<(u64, usize, bool)>,
        show_hidden: bool,
    ) {
        if !visited.insert(a.id) {
            return;
        }
        out.push((a.id.0, depth, a.hidden && !show_hidden));
        for child in sorted.iter().filter(|c| c.subagent_parent_id == Some(a.id)) {
            visit(child, depth + 1, sorted, visited, out, show_hidden);
        }
    }
    for a in sorted
        .iter()
        .filter(|a| a.subagent_parent_id.is_none_or(|p| !included.contains(&p)))
        .chain(sorted.iter())
    {
        visit(a, 0, &sorted, &mut visited, &mut out, show_hidden);
    }
    out
}

#[test]
fn ancestry_matches_original_for_all_small_cycles_orphans_and_hidden_combinations() {
    let template = agent(0, None, false, false);
    // None, self/other parents, and missing parents; every hidden combination.
    for graph in 0u32..125 {
        for hidden in 0..8 {
            let mut agents = Vec::new();
            let mut parents = graph;
            for id in 1..=3 {
                let mut agent = template.clone();
                agent.id = AgentId(id);
                agent.hidden = hidden & (1 << (id - 1)) != 0;
                agent.subagent_parent_id = match parents % 5 {
                    0 => None,
                    parent => Some(AgentId(u64::from(parent))),
                };
                parents /= 5;
                agents.push(agent);
            }
            // Traversal must not depend on input order.
            agents.reverse();
            for show_hidden in [false, true] {
                let actual: Vec<_> = agent_tree(&agents, show_hidden)
                    .into_iter()
                    .map(|r| (r.agent.id.0, r.depth, r.context_only))
                    .collect();
                assert_eq!(
                    actual,
                    reference_agent_tree(&agents, show_hidden),
                    "graph={graph}, hidden={hidden}, show_hidden={show_hidden}"
                );
            }
        }
    }
}

#[test]
fn ancestry_handles_deep_shared_chains_without_recursion() {
    let template = agent(0, None, false, false);
    let agents: Vec<_> = (1..=16_384)
        .rev()
        .map(|id| {
            let mut agent = template.clone();
            agent.id = AgentId(id);
            agent.subagent_parent_id = (id > 1).then(|| AgentId(id - 1));
            agent
        })
        .collect();
    let rows = agent_tree(&agents, false);
    assert_eq!(rows.len(), agents.len());
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.agent.id.0, index as u64 + 1);
        assert_eq!(row.depth, index);
        assert!(!row.context_only);
    }
    let mut hidden_chain = agents;
    for agent in &mut hidden_chain {
        agent.hidden = agent.id != AgentId(16_384);
    }
    let rows = agent_tree(&hidden_chain, false);
    assert_eq!(rows.len(), hidden_chain.len());
    assert!(rows[..rows.len() - 1].iter().all(|r| r.context_only));
    assert!(!rows.last().unwrap().context_only);
}

#[test]
fn ancestry_sorts_wide_siblings_and_omits_hidden_only_subtrees() {
    let template = agent(1, None, false, false);
    let mut agents = vec![template.clone()];
    for id in (2..=8192).rev() {
        let mut child = template.clone();
        child.id = AgentId(id);
        child.subagent_parent_id = Some(AgentId(1));
        child.hidden = id % 2 == 0;
        agents.push(child);
    }
    let rows = agent_tree(&agents, false);
    assert_eq!(rows.len(), 4096);
    assert_eq!(rows[0].agent.id, AgentId(1));
    for (index, row) in rows[1..].iter().enumerate() {
        assert_eq!(row.agent.id.0, index as u64 * 2 + 3);
        assert_eq!(row.depth, 1);
    }
}

/// Manual scaling diagnostic, deliberately without brittle wall-clock assertions.
#[test]
#[ignore = "manual projection scaling benchmark"]
fn benchmark_grouping_and_deep_ancestry_scaling() {
    use std::{hint::black_box, time::Instant};

    let session_template = session(0, "/unused");
    let agent_template = agent(0, None, false, false);
    for size in [1_000, 10_000, 100_000] {
        let mut index = SessionIndex::default();
        let mut agents = Vec::new();
        for id in 1..=size {
            let mut session = session_template.clone();
            session.id = SessionId(id);
            session.working_directory = format!("/work/{id}").into();
            index.upsert(session);
            let mut agent = agent_template.clone();
            agent.id = AgentId(id);
            agent.subagent_parent_id = (id > 1).then(|| AgentId(id - 1));
            agents.push(agent);
        }
        let start = Instant::now();
        assert_eq!(
            black_box(index.groups(&Default::default())).len(),
            size as usize
        );
        let grouping = start.elapsed();
        let start = Instant::now();
        assert_eq!(black_box(agent_tree(&agents, false)).len(), size as usize);
        eprintln!(
            "{size} rows: grouping={grouping:?}, ancestry={:?}",
            start.elapsed()
        );
    }
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
    use psa::api::events::*;
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

fn probe_filter(probe: psa::api::ProbeId) -> psa::api::SessionFilter {
    psa::api::SessionFilter {
        probe_ids: vec![probe],
        ..Default::default()
    }
}
