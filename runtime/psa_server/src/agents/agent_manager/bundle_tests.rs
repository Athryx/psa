use super::*;
use crate::{
    agents::{NewAgent, client_queries::ClientQueries, supervisor::SupervisorCommand},
    components::bundles::{
        BundleKind,
        testing::{CleanupBehavior, ProbeBundle, ProbeState, install_owned},
    },
    sessions::NewSession,
};
use std::{collections::VecDeque, panic::AssertUnwindSafe, sync::atomic::Ordering};

async fn create_agent_record(
    definition: crate::db::CreateAgentDefinition,
) -> (tempfile::TempDir, Database, Agent) {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("bundle-worker.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&database).await;
    let mut cursor = database.write().await.unwrap();
    let definition = cursor.create_agent_definition(definition).await.unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            name: "bundle worker".into(),
            working_directory: "/work".into(),
            probe_id: crate::test_support::probe_metadata().id,
            project_id: None,
            model_config: crate::test_support::model_config(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                definition_id: definition.id,
                name: "bundle agent".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    (directory, database, agent)
}

async fn install_probe_bundle(
    state: &mut AgentManagerState,
    shared: Arc<ProbeState>,
    cleanup: CleanupBehavior,
) -> Arc<Mutex<InstalledComponents>> {
    let mut bundle = ProbeBundle::new("worker", shared);
    bundle.components = true;
    bundle.cleanup = cleanup;
    let components = install_owned(
        vec![BundleKind::Probe(bundle)],
        state.agent(),
        state.session(),
    )
    .await
    .unwrap();
    let components = Arc::new(Mutex::new(components));
    state.attach_components(components.clone()).await;
    components
}

async fn run_test_worker(
    state: AgentManagerState,
    boundaries: Option<Vec<BoundaryReply>>,
) -> std::thread::Result<Result<(), AgentError>> {
    let (commands, mut received) = actors::channel::<SupervisorCommand>(8);
    let manager = SupervisorClient::new(commands);
    let mut client_queries = ClientQueries::start(state.publication.clone());
    let sessions = RuntimeHandle::new(
        state.db.clone(),
        state.publication.clone(),
        manager.clone(),
        client_queries.handle(),
    );
    let supervisor = boundaries.map(|boundaries| {
        let mut boundaries = VecDeque::from(boundaries);
        tokio::spawn(async move {
            while let Some(command) = received.recv().await {
                match command {
                    SupervisorCommand::Boundary { reply, .. } => {
                        let _ = reply.send(boundaries.pop_front().unwrap_or(BoundaryReply::Stop));
                    }
                    _ => panic!("unexpected bundle test supervisor command"),
                }
            }
        })
    });
    let (_controls, controls) = actors::unbounded_channel();
    let probes = ProbeRegistry::start("bundle-test".into());
    let outcome = AssertUnwindSafe(run_worker(
        state,
        1,
        manager,
        controls,
        ProviderRegistry::default(),
        probes.clone(),
        sessions,
    ))
    .catch_unwind()
    .await;

    client_queries.shutdown().await.unwrap();
    probes.shutdown().await;
    if let Some(supervisor) = supervisor {
        supervisor.await.unwrap();
    }
    outcome
}

fn assert_components_released_before_cleanup(shared: &ProbeState) {
    let events = shared.events();
    let cleanup = events
        .iter()
        .position(|event| event == "cleanup:worker")
        .unwrap();
    let dropped = events
        .iter()
        .position(|event| event == "drop:worker")
        .unwrap();
    assert!(cleanup < dropped, "{events:?}");
    for component in [
        "tool_drop:probe_first",
        "tool_drop:probe_second",
        "hook_drop",
    ] {
        let position = events.iter().position(|event| event == component).unwrap();
        assert!(position < cleanup, "{events:?}");
    }
}

#[tokio::test]
async fn normal_worker_retirement_retains_components_across_generations() {
    let (_directory, database, agent) = create_agent_record(crate::db::CreateAgentDefinition {
        name: "normal".into(),
        ..Default::default()
    })
    .await;
    let mut state = AgentManagerState::load(database.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    let shared = Arc::new(ProbeState::default());
    let components = install_probe_bundle(&mut state, shared.clone(), CleanupBehavior::Ok).await;
    {
        let mut installed = components.lock().await;
        let hook = installed.hooks.before_request.pop().unwrap();
        installed.hooks.agent_create.push(hook);
    }
    let tools = Arc::downgrade(&state.tools);

    for generation in 0..2 {
        run_test_worker(
            state,
            Some(vec![
                BoundaryReply::Hook(LifecycleHook::AgentCreate),
                BoundaryReply::Stop,
            ]),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(shared.events().is_empty());
        assert_eq!(shared.value.load(Ordering::SeqCst), generation + 1);
        if generation == 1 {
            break;
        }
        state =
            AgentManagerState::load_persisted(database.clone(), Publication::default(), agent.id)
                .await
                .unwrap();
        state.attach_components(components.clone()).await;
        assert!(Arc::ptr_eq(&state.tools, &tools.upgrade().unwrap()));
    }
    components.lock().await.cleanup().await.unwrap();
    assert_components_released_before_cleanup(&shared);
    database.close().await;
}

#[tokio::test]
async fn worker_supervisor_error_retains_bundle_until_owner_cleanup() {
    let (_directory, database, agent) = create_agent_record(crate::db::CreateAgentDefinition {
        name: "error".into(),
        ..Default::default()
    })
    .await;
    let mut state = AgentManagerState::load(database.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    let shared = Arc::new(ProbeState::default());
    let components = install_probe_bundle(&mut state, shared.clone(), CleanupBehavior::Ok).await;

    let result = run_test_worker(state, None).await.unwrap();
    assert!(matches!(result, Err(AgentError::SupervisorUnavailable)));
    assert!(shared.events().is_empty());
    components.lock().await.cleanup().await.unwrap();
    assert_components_released_before_cleanup(&shared);
    database.close().await;
}

#[tokio::test]
async fn lifecycle_hook_error_and_panic_retain_bundle_until_owner_cleanup() {
    for panic in [false, true] {
        let (_directory, database, agent) = create_agent_record(crate::db::CreateAgentDefinition {
            name: "hook failure".into(),
            ..Default::default()
        })
        .await;
        let mut state = AgentManagerState::load(database.clone(), Publication::default(), agent.id)
            .await
            .unwrap();
        let shared = Arc::new(ProbeState::default());
        shared.hook_panics.store(panic, Ordering::SeqCst);
        shared.hook_fails.store(!panic, Ordering::SeqCst);
        let components =
            install_probe_bundle(&mut state, shared.clone(), CleanupBehavior::Ok).await;
        {
            let mut installed = components.lock().await;
            let hook = installed.hooks.before_request.pop().unwrap();
            installed.hooks.agent_create.push(hook);
        }

        let result = run_test_worker(
            state,
            Some(vec![BoundaryReply::Hook(LifecycleHook::AgentCreate)]),
        )
        .await;
        if panic {
            assert!(result.is_err());
        } else {
            assert!(matches!(result.unwrap(), Err(AgentError::Hook(_))));
        }
        assert!(shared.events().is_empty());
        components.lock().await.cleanup().await.unwrap();
        assert_components_released_before_cleanup(&shared);
        database.close().await;
    }
}

#[tokio::test]
async fn cleanup_errors_are_returned_by_owner_not_worker_retirement() {
    let (_directory, database, agent) = create_agent_record(crate::db::CreateAgentDefinition {
        name: "cleanup failure".into(),
        ..Default::default()
    })
    .await;
    let mut state = AgentManagerState::load(database.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    let shared = Arc::new(ProbeState::default());
    let components = install_probe_bundle(&mut state, shared.clone(), CleanupBehavior::Error).await;

    let result = run_test_worker(state, Some(vec![BoundaryReply::Stop]))
        .await
        .unwrap();
    result.unwrap();
    assert!(shared.events().is_empty());
    assert!(components.lock().await.cleanup().await.is_err());
    assert_components_released_before_cleanup(&shared);
    database.close().await;
}

#[tokio::test]
async fn replay_and_recovery_do_not_install_worker_bundles() {
    // This definition deliberately cannot install. Recovery must still operate
    // on its persisted transcript without constructing bundle resources.
    let (_directory, database, agent) = create_agent_record(crate::db::CreateAgentDefinition {
        name: "duplicate file tool".into(),
        bundles: vec![
            psa::api::BundleConfig::FileTools { read_only: false },
            psa::api::BundleConfig::FileTools { read_only: true },
        ],
        ..Default::default()
    })
    .await;
    assert!(matches!(
        AgentManagerState::load(database.clone(), Publication::default(), agent.id).await,
        Err(AgentError::BundleInstall(_))
    ));
    recover_agent(&database, &Publication::default(), agent.id)
        .await
        .unwrap();
    let state =
        AgentManagerState::load_persisted(database.clone(), Publication::default(), agent.id)
            .await
            .unwrap();
    assert!(state.tools().is_empty());
    drop(state);
    database.close().await;
}
