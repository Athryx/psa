use super::*;
use crate::{
    agents::NewAgent,
    components::bundles::{
        BundleKind,
        testing::{CleanupBehavior, ProbeBundle, ProbeState, install_owned},
    },
    sessions::NewSession,
};

#[tokio::test]
async fn runtime_shutdown_cleans_retained_owners_and_reports_failures() {
    for behavior in [
        CleanupBehavior::Ok,
        CleanupBehavior::Error,
        CleanupBehavior::Panic,
        CleanupBehavior::Pending,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open(directory.path().join("owners.sqlite3"))
            .await
            .unwrap();
        crate::test_support::register_probe(&db).await;
        let mut cursor = db.cursor().await.unwrap();
        let definition = cursor
            .create_agent_definition(psa_api::CreateAgentDefinition {
                name: "owner".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = cursor
            .create_session(NewSession {
                name: "owners".into(),
                working_directory: "/work".into(),
                probe_id: crate::test_support::probe_metadata().id,
                model_config: crate::test_support::model_config(),
            })
            .await
            .unwrap();
        let mut agents = Vec::new();
        for name in ["first", "second"] {
            agents.push(
                cursor
                    .create_agent(
                        session.id,
                        NewAgent {
                            definition_id: definition.id,
                            name: name.into(),
                            interactive: false,
                        },
                    )
                    .await
                    .unwrap(),
            );
        }
        cursor.commit().await.unwrap();

        let publication = Publication::default();
        let probes = ProbeRegistry::start("owner-test".into());
        let (commands, received) = actors::channel(256);
        let commands = ManagerClient::new(commands);
        let client_queries = ClientQueries::start(publication.clone());
        let sessions = Sessions::new(
            db.clone(),
            publication.clone(),
            commands.clone(),
            client_queries.handle(),
        );
        let mut manager = SessionManager {
            db: db.clone(),
            publication,
            providers: ProviderRegistry::default(),
            probes: probes.clone(),
            sessions: sessions.clone(),
            client_queries,
            commands,
            received,
            workers: HashMap::new(),
            components: HashMap::new(),
            session_resources: HashMap::new(),
            steering: HashMap::new(),
            runs: HashMap::new(),
            lifecycle_events: Vec::new(),
            tasks: JoinSet::new(),
            next_request_id: 1,
            next_generation: 1,
            shutting_down: false,
            shutdown_reply: None,
            cleanup_failed: false,
        };
        let mut states = Vec::new();
        for agent in agents {
            let shared = Arc::new(ProbeState::default());
            let mut bundle = ProbeBundle::new(agent.name.clone(), shared.clone());
            bundle.cleanup = behavior;
            // A stuck agent must not delay attempting other agent cleanups.
            bundle.timeout = std::time::Duration::from_millis(20);
            let components = install_owned(vec![BundleKind::Probe(bundle)], &agent, &session)
                .await
                .unwrap();
            manager
                .components
                .insert(agent.id, Arc::new(Mutex::new(components)));
            states.push((agent.name, shared));
        }
        if matches!(behavior, CleanupBehavior::Ok) {
            let agent_ids: Vec<_> = manager.components.keys().copied().collect();
            assert!(!Arc::ptr_eq(
                &manager.components[&agent_ids[0]],
                &manager.components[&agent_ids[1]]
            ));
            for agent in agent_ids {
                let retained = Arc::clone(&manager.components[&agent]);
                for _ in 0..2 {
                    manager
                        .spawn_worker(agent, session.id, VecDeque::new(), VecDeque::new())
                        .await
                        .unwrap();
                    let boundary = manager.received.recv().await.unwrap();
                    assert!(matches!(&boundary, ManagerCommand::Boundary { .. }));
                    manager.handle_command(boundary).await;
                    let joined = manager.tasks.join_next_with_id().await.unwrap();
                    manager.handle_join(joined).await;
                    assert!(!manager.workers.contains_key(&agent));
                    assert!(Arc::ptr_eq(&retained, &manager.components[&agent]));
                }
            }
            assert!(states.iter().all(|(_, state)| state.events().is_empty()));
        }
        let runtime = Runtime {
            handle: sessions,
            task: tokio::spawn(manager.run()),
        };
        let result = runtime.shutdown().await;
        if matches!(behavior, CleanupBehavior::Ok) {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(RuntimeError::CleanupFailed)));
        }
        for (name, state) in states {
            assert_eq!(
                state.events(),
                [format!("cleanup:{name}"), format!("drop:{name}")]
            );
        }
        probes.shutdown().await;
        db.close().await;
    }
}
