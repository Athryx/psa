//! Runs built-in bundles through the runtime's creation and session-update
//! lifecycle.

use crate::bundles::BuiltinComponents;
use psa::api::{AgentId, events::EventType};
use psa_agents::supervisor::testing::lifecycle::{Fixture, WAIT, agent_request, rename};
use psa_store::Database;

async fn identity_prompt(database: &Database, agent: AgentId) -> String {
    let mut cursor = database.read().await.unwrap();
    let events = cursor.load_all_events(agent).await.unwrap();
    cursor.close().await.unwrap();
    assert_eq!(
        serde_json::to_string(&events)
            .unwrap()
            .matches("<agent_identity>")
            .count(),
        1
    );

    let projection = psa_agents::agent_projection::AgentProjection::replay(&events).unwrap();
    let prompts = projection
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            llm_api::api::HistoryItemKind::Message(message) => Some(&message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            llm_api::api::ContentBlock::Text(text) if text.text.starts_with("<agent_identity>") => {
                Some(text.text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prompts.len(),
        1,
        "identity must survive startup initialization"
    );
    prompts.into_iter().next().unwrap()
}

#[tokio::test]
async fn subagent_identity_is_created_once_and_preserved_on_reload_restart_and_fork() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start_with(BuiltinComponents).await;
    let definition = runtime
        .handle
        .create_agent_definition(psa_store::CreateAgentDefinition {
            name: "subagents".into(),
            bundles: vec![
                psa::api::BundleConfig::Startup {
                    base_prompt: true,
                    project_context: false,
                    cwd: false,
                    skills: false,
                },
                psa::api::BundleConfig::Subagents,
                psa::api::BundleConfig::StopWithoutTools,
            ],
            ..Default::default()
        })
        .await
        .unwrap();
    let session = fixture.create_session(&runtime.handle).await;
    let agent = runtime
        .handle
        .create_agent(
            session.id,
            psa_agents::NewAgent {
                definition_id: definition.id,
                ..agent_request()
            },
        )
        .await
        .unwrap();
    fixture
        .next("create", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    let identity = identity_prompt(&fixture.db, agent.id).await;
    assert!(identity.contains(&format!("You are agent \"{}\"", agent.name)));
    assert!(!identity.contains("Agent ID"));

    // Configured startup initializes context before the bundle's create hook.
    let mut resources = crate::bundles::session::SessionBundleResources::start(
        session.id,
        runtime.handle.clone(),
        None,
    );
    let mut installed =
        crate::bundles::install_bundles(&definition, &agent, &session, Some(&resources.handle()))
            .await
            .unwrap();
    assert_eq!(installed.hooks.agent_create[0].name(), "DefaultStartup");
    assert_eq!(installed.hooks.agent_create[2].name(), "SubagentIdentity");
    assert!(installed.hooks.before_request.is_empty());
    installed.cleanup().await.unwrap();
    resources.cleanup().await.unwrap();

    rename(&runtime.handle, session.id, "reloaded").await;
    fixture
        .next("update", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert_eq!(identity_prompt(&fixture.db, agent.id).await, identity);
    runtime.shutdown().await.unwrap();

    let runtime = fixture.start_with(BuiltinComponents).await;
    rename(&runtime.handle, session.id, "restarted").await;
    fixture
        .next("update", agent.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;
    assert_eq!(identity_prompt(&fixture.db, agent.id).await, identity);

    let fork = runtime
        .handle
        .fork_session(
            session.id,
            psa::api::ForkSession {
                point: psa::api::ForkPoint::Head,
                metadata: psa::api::UpdateSession::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0];
    fixture
        .next("create", child.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, fork.session.id).await;
    assert_ne!(child.id, agent.id);
    assert_eq!(identity_prompt(&fixture.db, child.id).await, identity);
    assert!(fixture.recorded.try_recv().is_err());

    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}

#[tokio::test]
async fn startup_inherits_session_model_and_updates_leave_explicit_definition_model_pinned() {
    let mut fixture = Fixture::new(false).await;
    let runtime = fixture.start_with(BuiltinComponents).await;
    let session = fixture.create_session(&runtime.handle).await;
    let inherited = runtime
        .handle
        .create_agent(session.id, agent_request())
        .await
        .unwrap();
    fixture
        .next("create", inherited.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut explicit_model = session.model_config.clone();
    explicit_model.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "explicit-model",
    )
    .unwrap();
    let definition = runtime
        .handle
        .create_agent_definition(psa_store::CreateAgentDefinition {
            name: "explicit".into(),
            model: explicit_model.clone().into(),
            bundles: vec![
                psa::api::BundleConfig::Startup {
                    base_prompt: true,
                    project_context: false,
                    cwd: false,
                    skills: false,
                },
                psa::api::BundleConfig::StopWithoutTools,
            ],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_ne!(definition.id, inherited.definition_id);
    let explicit = runtime
        .handle
        .create_agent(
            session.id,
            psa_agents::NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                name: "explicit".into(),
                definition_id: definition.id,
            },
        )
        .await
        .unwrap();
    fixture
        .next("create", explicit.id)
        .await
        .release
        .send(())
        .unwrap();
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut cursor = fixture.db.read().await.unwrap();
    for (agent, expected) in [
        (&inherited, &session.model_config),
        (&explicit, &explicit_model),
    ] {
        let events = cursor.load_all_events(agent.id).await.unwrap();
        // Startup always begins with a new context, the model, and base instructions.
        assert!(events.len() >= 3);
        assert!(matches!(events[0].kind, EventType::NewContext));
        assert!(matches!(&events[1].kind, EventType::Model(model) if model == expected));
        assert!(
            matches!(&events[2].kind, EventType::Message(message) if message.message_type == psa::api::events::MessageType::System)
        );
    }
    cursor.close().await.unwrap();

    let mut updated_model = session.model_config.clone();
    updated_model.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "updated-model",
    )
    .unwrap();
    runtime
        .handle
        .update_session(
            session.id,
            psa::api::UpdateSession {
                model_config: Some(updated_model.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut updated_agents = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(WAIT, fixture.recorded.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "update");
        assert_eq!(event.session.model_config, updated_model);
        updated_agents.push(event.agent);
        event.release.send(()).unwrap();
    }
    assert!(updated_agents.contains(&inherited.id));
    assert!(updated_agents.contains(&explicit.id));
    fixture.wait_idle(&runtime.handle, session.id).await;

    runtime
        .handle
        .update_session(
            session.id,
            psa::api::UpdateSession {
                name: Some("renamed without changing model".into()),
                model_config: Some(updated_model.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut unchanged_agents = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(WAIT, fixture.recorded.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "update");
        assert_eq!(event.session.model_config, updated_model);
        unchanged_agents.push(event.agent);
        event.release.send(()).unwrap();
    }
    assert!(unchanged_agents.contains(&inherited.id));
    assert!(unchanged_agents.contains(&explicit.id));
    fixture.wait_idle(&runtime.handle, session.id).await;

    let mut cursor = fixture.db.read().await.unwrap();
    for (agent, expected, model_count) in [
        (&inherited, &updated_model, 2),
        (&explicit, &explicit_model, 1),
    ] {
        let events = cursor.load_all_events(agent.id).await.unwrap();
        let models: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventType::Model(model) => Some(model),
                _ => None,
            })
            .collect();
        assert_eq!(models.len(), model_count);
        assert_eq!(*models.last().unwrap(), expected);
    }
    cursor.close().await.unwrap();
    runtime.shutdown().await.unwrap();
    fixture.probes.shutdown().await;
}
