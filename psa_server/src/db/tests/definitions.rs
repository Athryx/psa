use super::{create_legacy_database, fixture, model};
use crate::{
    agents::NewAgent,
    db::{Database, DbError},
    sessions::{ForkPoint, SessionFork, SessionUpdate},
};
use psa_api::{AgentDefinitionId, BundleConfig, CreateAgentDefinition, HookConfig, HookConfigs};

#[tokio::test]
async fn definitions_roundtrip_literal_values_and_insert_immutable_versions() {
    let (dir, db, session, _) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let request = CreateAgentDefinition {
        name: " \n literal name \t".into(),
        description: "description\n\n".into(),
        selectable: false,
        bundles: vec![
            BundleConfig::FileTools { read_only: false },
            BundleConfig::FileTools { read_only: true },
            BundleConfig::Noop,
            BundleConfig::Noop,
        ],
        hooks: HookConfigs {
            agent_create: vec![
                HookConfig::Noop,
                HookConfig::DefaultStartup,
                HookConfig::Noop,
            ],
            session_update: vec![HookConfig::SessionModelUpdate],
            before_request: vec![
                HookConfig::Noop,
                HookConfig::ExtraSystemPrompt {
                    prompt: " \n literal prompt \n ".into(),
                },
                HookConfig::Noop,
            ],
            after_request: vec![HookConfig::StopWithoutTools, HookConfig::Noop],
        },
        model_config: Some(model()),
        skills: vec![
            "relative/../skill".into(),
            "/missing/skill".into(),
            "relative/../skill".into(),
        ],
        permitted_subagents: vec![AgentDefinitionId(999999), AgentDefinitionId(999999)],
    };
    let first = cursor
        .create_agent_definition(request.clone())
        .await
        .unwrap();
    let mut expected = serde_json::to_value(&request).unwrap();
    let object = expected.as_object_mut().unwrap();
    object.insert("id".into(), serde_json::json!(first.id));
    object.insert("version".into(), serde_json::json!(1));
    object.insert("previous_version_id".into(), serde_json::Value::Null);
    assert_eq!(serde_json::to_value(&first).unwrap(), expected);
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                name: "pinned".into(),
                definition_id: first.id,
            },
        )
        .await
        .unwrap();

    let replacement = CreateAgentDefinition {
        name: String::new(),
        ..Default::default()
    };
    let next = cursor
        .update_agent_definition(first.id, replacement)
        .await
        .unwrap();
    assert_ne!(first.id, next.id);
    assert_eq!(next.previous_version_id, Some(first.id));
    assert_eq!(next.version, 2);
    assert!(next.bundles.is_empty());
    assert!(next.hooks.agent_create.is_empty());
    assert!(next.model_config.is_none());
    assert!(next.hooks.before_request.is_empty());
    assert_eq!(
        serde_json::to_value(cursor.get_agent_definition(first.id).await.unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().definition_id,
        first.id
    );
    assert_eq!(cursor.list_agent_definitions().await.unwrap().len(), 3);
    let missing = AgentDefinitionId(777777);
    assert!(matches!(
        cursor.get_agent_definition(missing).await,
        Err(DbError::NotFound { .. })
    ));
    assert!(matches!(
        cursor
            .update_agent_definition(missing, Default::default())
            .await,
        Err(DbError::NotFound { .. })
    ));
    assert!(
        cursor
            .create_agent(
                session.id,
                NewAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    name: "invalid reference".into(),
                    definition_id: missing
                }
            )
            .await
            .is_err()
    );
    cursor.commit().await.unwrap();
    db.close().await;

    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_agent_definition(first.id).await.unwrap().bundles,
        request.bundles
    );
    assert!(
        cursor
            .get_agent_definition(next.id)
            .await
            .unwrap()
            .bundles
            .is_empty()
    );
    let definitions = cursor.list_agent_definitions().await.unwrap();
    assert_eq!(
        definitions
            .iter()
            .find(|definition| definition.id == first.id)
            .unwrap()
            .bundles,
        request.bundles
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn migration_defaults_legacy_definitions_to_empty_bundles_and_checks_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite3");
    let pool = create_legacy_database(&path).await;
    pool.close().await;

    let db = Database::open(&path).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let definition = cursor
        .get_agent_definition(AgentDefinitionId(1))
        .await
        .unwrap();
    assert!(definition.bundles.is_empty());
    let stored: String = sqlx::query_scalar("SELECT bundles FROM agent_definitions WHERE id = 1")
        .fetch_one(cursor.connection())
        .await
        .unwrap();
    assert_eq!(stored, "[]");
    for value in [Some("not-json"), None] {
        assert!(
            sqlx::query("UPDATE agent_definitions SET bundles = ? WHERE id = 1")
                .bind(value)
                .execute(cursor.connection())
                .await
                .is_err()
        );
    }
    cursor.commit().await.unwrap();
    db.close().await;

    let db = Database::open(&path).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert!(
        cursor
            .get_agent_definition(AgentDefinitionId(1))
            .await
            .unwrap()
            .bundles
            .is_empty()
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn fork_preserves_definition_reference_and_session_model_or_explicit_override() {
    let (_dir, db, session, agent) = fixture().await;
    let mut cursor = db.cursor().await.unwrap();
    let updated_definition = cursor
        .update_agent_definition(
            agent.definition_id,
            CreateAgentDefinition {
                name: "updated".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.session.model_config, session.model_config);
    assert_eq!(fork.agents[0].definition_id, agent.definition_id);
    assert_ne!(fork.agents[0].definition_id, updated_definition.id);

    let mut changed = model();
    changed.model = llm_api::providers::Model::new(
        llm_api::providers::ProviderType::Openrouter,
        "different-model",
    )
    .unwrap();
    let overridden = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::Head,
                metadata: SessionUpdate {
                    model_config: Some(changed.clone()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(overridden.session.model_config, changed);
    assert_eq!(
        cursor.get_session(session.id).await.unwrap().model_config,
        session.model_config
    );
    assert_eq!(
        cursor
            .get_agent(overridden.agents[0].id)
            .await
            .unwrap()
            .definition_id,
        agent.definition_id
    );
    let updated = cursor
        .update_session(
            session.id,
            SessionUpdate {
                model_config: Some(changed.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.model_config, changed);
    assert_eq!(
        cursor
            .get_session(fork.session.id)
            .await
            .unwrap()
            .model_config,
        session.model_config
    );
    cursor.commit().await.unwrap();
    db.close().await;
}
