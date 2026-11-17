use super::*;
use crate::db::Database;

fn id(name: &str) -> ConfigId {
    ConfigId {
        config_group: ConfigGroup::Global,
        name: name.into(),
    }
}
fn config(name: &str, children: &[&str]) -> AgentConfig {
    AgentConfig {
        config_id: id(name),
        description: name.into(),
        selectable: true,
        bundles: vec![],
        hooks: HookConfigs::default(),
        models: vec![ModelSelection::Global],
        default: true,
        subagents: IncludedItems {
            groups: vec![],
            items: children.iter().map(|name| id(name)).collect(),
        },
        skills: IncludedItems::default(),
    }
}
fn skill(name: &str) -> SkillRegistration {
    SkillRegistration {
        config_id: id(name),
        name: name.into(),
        description: name.into(),
        skill_location: SkillLocation::Global(PathBuf::from(format!("/skills/{name}"))),
        disable_model_invocation: false,
    }
}
async fn database() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("registry.sqlite3"))
        .await
        .unwrap();
    (dir, db)
}

#[tokio::test]
async fn cycles_forward_references_dedup_and_transitive_updates() {
    let (_dir, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor
        .upsert_agent_configs(vec![
            config("a", &["b"]),
            config("b", &["c"]),
            config("c", &["b"]),
        ])
        .await
        .unwrap();
    let first = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let b = cursor
        .get_agent_definition(first.references.subagents[0])
        .await
        .unwrap();
    let c = cursor
        .get_agent_definition(b.permitted_subagents[0])
        .await
        .unwrap();
    assert_eq!(c.permitted_subagents, vec![b.id]);
    let repeated = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_eq!(first.definition.id, repeated.definition.id);
    // Resolving a component as a root reuses its already-published cyclic snapshot.
    let root_b = cursor
        .resolve_agent_config(&id("b"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_eq!(root_b.definition.id, b.id);
    let mut changed = config("c", &["b"]);
    changed.description = "changed".into();
    cursor.upsert_agent_configs(vec![changed]).await.unwrap();
    let second = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_ne!(first.definition.id, second.definition.id);
    assert_ne!(first.references.subagents, second.references.subagents);
    assert_eq!(
        cursor.get_agent_definition(c.id).await.unwrap().description,
        "c"
    );
    assert_eq!(
        cursor
            .get_agent_definition(first.definition.id)
            .await
            .unwrap()
            .permitted_subagents,
        vec![b.id]
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn missing_prunes_restoration_and_mutable_skill_identity() {
    let (_dir, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    let mut a = config("a", &["missing"]);
    a.skills.items = vec![id("s")];
    cursor.upsert_agent_configs(vec![a.clone()]).await.unwrap();
    assert_eq!(
        cursor
            .get_agent_config(&id("a"))
            .await
            .unwrap()
            .status
            .missing_components
            .len(),
        2
    );
    let empty = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert!(empty.references.skills.is_empty());
    let registered = cursor
        .upsert_skills(vec![skill("s")])
        .await
        .unwrap()
        .remove(0);
    cursor
        .upsert_agent_configs(vec![config("missing", &[])])
        .await
        .unwrap();
    let paths = HashMap::from([(registered.id, PathBuf::from("/probe/s"))]);
    assert!(
        cursor
            .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
            .await
            .is_err()
    );
    let full = cursor
        .resolve_agent_config(&id("a"), &paths, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(full.references.skills, vec![registered.id]);
    assert_ne!(empty.definition.id, full.definition.id);
    let mut updated = skill("s");
    updated.description = "new metadata".into();
    let refreshed = cursor.upsert_skills(vec![updated]).await.unwrap().remove(0);
    assert_eq!(registered.id, refreshed.id);
    assert_eq!(
        full.definition.id,
        cursor
            .resolve_agent_config(&id("a"), &paths, &HashMap::new())
            .await
            .unwrap()
            .definition
            .id
    );
    cursor
        .replace_config_group(&ConfigGroup::Global, vec![a.clone()], vec![])
        .await
        .unwrap();
    assert_eq!(
        cursor
            .get_agent_config(&id("a"))
            .await
            .unwrap()
            .status
            .missing_components
            .len(),
        2
    );
    assert_eq!(
        empty.definition.id,
        cursor
            .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
            .await
            .unwrap()
            .definition
            .id
    );
    assert!(cursor.list_skills().await.unwrap().is_empty());
    assert_eq!(
        cursor.list_skill_sources().await.unwrap()[0].id,
        registered.id
    );
    let pinned_pruned = cursor
        .resolve_pinned_agent_config(full.definition.id, AgentModel::Global, &HashMap::new())
        .await
        .unwrap();
    assert!(pinned_pruned.references.skills.is_empty());
    assert_eq!(
        pinned_pruned.references.subagents,
        full.references.subagents
    );
    assert_eq!(
        cursor
            .get_agent_definition(full.definition.id)
            .await
            .unwrap()
            .skills,
        vec![PathBuf::from("/probe/s")]
    );
    let restored = cursor
        .upsert_skills(vec![skill("s")])
        .await
        .unwrap()
        .remove(0);
    assert_eq!(restored.id, registered.id);
    cursor.commit().await.unwrap();
    db.close().await;
    let reopened = Database::open(_dir.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.cursor().await.unwrap();
    assert_eq!(
        cursor.get_skill(registered.id).await.unwrap().id,
        registered.id
    );
    assert_eq!(
        cursor
            .get_resolved_agent_config(full.definition.id)
            .await
            .unwrap()
            .unwrap()
            .references
            .skills,
        vec![registered.id]
    );
    assert_eq!(
        cursor
            .get_agent_config(&id("a"))
            .await
            .unwrap()
            .status
            .missing_components
            .len(),
        1
    );
    cursor.commit().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn hook_prompt_payload_roundtrips_and_changes_snapshot_identity() {
    let (directory, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    let mut initial = config("prompt-payload", &[]);
    initial.hooks.agent_create = vec![psa_api::HookConfig::ExtraSystemPrompt {
        prompt: "  Original instructions.\n<literal> & text\n".into(),
    }];
    cursor
        .upsert_agent_configs(vec![initial.clone()])
        .await
        .unwrap();
    let first = cursor
        .resolve_agent_config(&initial.config_id, &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let cached = cursor
        .resolve_agent_config(&initial.config_id, &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_eq!(cached.definition.id, first.definition.id);
    assert_eq!(first.definition.hooks, initial.hooks);
    assert_eq!(first.config.hooks, initial.hooks);

    let mut changed = initial.clone();
    changed.hooks.agent_create = vec![psa_api::HookConfig::ExtraSystemPrompt {
        prompt: "Different instructions.".into(),
    }];
    cursor
        .upsert_agent_configs(vec![changed.clone()])
        .await
        .unwrap();
    let second = cursor
        .resolve_agent_config(&changed.config_id, &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_ne!(second.definition.id, first.definition.id);
    assert_eq!(second.definition.hooks, changed.hooks);
    let retained = cursor
        .get_resolved_agent_config(first.definition.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.definition.hooks, initial.hooks);
    assert_eq!(retained.config.hooks, initial.hooks);
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_agent_definition(first.definition.id)
            .await
            .unwrap()
            .hooks,
        initial.hooks
    );
    assert_eq!(
        cursor
            .get_agent_definition(second.definition.id)
            .await
            .unwrap()
            .hooks,
        changed.hooks
    );
    cursor.rollback().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn single_model_defaults_are_normalized_for_api_and_discovered_registration() {
    let (directory, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    let mut api = config("single-api", &[]);
    api.default = false;
    let mut multiple = config("multiple-api", &[]);
    multiple.models = vec![ModelSelection::Global, ModelSelection::Parent];
    multiple.default = false;
    let registered = cursor
        .upsert_agent_configs(vec![api.clone(), multiple])
        .await
        .unwrap();
    assert!(registered[0].config.default);
    assert!(!registered[1].config.default);
    assert!(
        !api.default,
        "registration must not mutate the caller's input"
    );

    let mut discovered = config("single-discovered", &[]);
    discovered.models = vec![ModelSelection::Parent];
    discovered.default = false;
    let mut discovered_multiple = config("multiple-discovered", &[]);
    discovered_multiple.models = vec![ModelSelection::Global, ModelSelection::Parent];
    discovered_multiple.default = false;
    cursor
        .replace_discovered_group(
            &ConfigGroup::Global,
            vec![discovered, discovered_multiple],
            vec![],
        )
        .await
        .unwrap();
    assert!(
        cursor
            .get_agent_config(&id("single-discovered"))
            .await
            .unwrap()
            .config
            .default
    );
    assert!(
        !cursor
            .get_agent_config(&id("multiple-discovered"))
            .await
            .unwrap()
            .config
            .default
    );
    cursor.commit().await.unwrap();
    db.close().await;

    let reopened = Database::open(directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let mut cursor = reopened.cursor().await.unwrap();
    for name in ["single-api", "single-discovered"] {
        assert!(
            cursor
                .get_agent_config(&id(name))
                .await
                .unwrap()
                .config
                .default
        );
    }
    for name in ["multiple-api", "multiple-discovered"] {
        assert!(
            !cursor
                .get_agent_config(&id(name))
                .await
                .unwrap()
                .config
                .default
        );
    }
    cursor.rollback().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn atomic_batches_group_precedence_and_discovery_ownership() {
    let (_dir, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor
        .upsert_agent_configs(vec![config("api", &[])])
        .await
        .unwrap();
    cursor
        .replace_discovered_group(&ConfigGroup::Global, vec![config("source", &[])], vec![])
        .await
        .unwrap();
    cursor
        .replace_discovered_group(&ConfigGroup::Global, vec![], vec![])
        .await
        .unwrap();
    assert_eq!(cursor.list_agent_configs().await.unwrap().len(), 1);
    assert!(
        cursor
            .upsert_agent_configs(vec![config("x", &[]), config("x", &[])])
            .await
            .is_err()
    );
    let mut invalid = config("invalid", &[]);
    invalid.models.clear();
    assert!(
        cursor
            .upsert_agent_configs(vec![config("valid", &[]), invalid])
            .await
            .is_err()
    );
    assert!(cursor.get_agent_config(&id("valid")).await.is_err());
    // A SQL error halfway through the batch must also roll back, even if the
    // caller catches the error and commits its outer cursor.
    sqlx::query("CREATE TEMP TRIGGER reject_bad_config BEFORE INSERT ON agent_configs WHEN json_extract(NEW.payload, '$.config_id.name')='bad' BEGIN SELECT RAISE(ABORT, 'test failure'); END")
        .execute(cursor.connection()).await.unwrap();
    assert!(
        cursor
            .upsert_agent_configs(vec![config("partial", &[]), config("bad", &[])])
            .await
            .is_err()
    );
    assert!(cursor.get_agent_config(&id("partial")).await.is_err());
    let mut all = config("all", &[]);
    all.subagents.groups = vec![ConfigGroup::Global];
    cursor.upsert_agent_configs(vec![all]).await.unwrap();
    let resolved = cursor
        .resolve_agent_config(&id("all"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_eq!(resolved.references.subagents.len(), 1); // self excluded from group selection
    let explicit = IncludedItems {
        groups: vec![ConfigGroup::Global],
        items: vec![ConfigId {
            config_group: ConfigGroup::Custom("override".into()),
            name: "api".into(),
        }],
    };
    assert_eq!(
        select_items(&explicit, &[id("api")], None)[0].config_group,
        ConfigGroup::Custom("override".into())
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn deferred_project_discovery_distinguishes_unknown_from_known_absent() {
    let (_dir, db) = database().await;
    crate::test_support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let probe_id = psa_api::ProbeId(psa_api::probes::Uuid::nil());
    let session = cursor
        .create_session(crate::sessions::NewSession {
            decision_model: None,
            name: "deferred".into(),
            working_directory: "/work".into(),
            probe_id,
            project_id: None,
            model_config: crate::test_support::model_config(),
        })
        .await
        .unwrap();
    assert!(
        !cursor
            .has_session_agents_dir_discovery(session.id)
            .await
            .unwrap()
    );
    assert_eq!(
        cursor.get_session_agents_dir(session.id).await.unwrap(),
        None
    );
    cursor
        .set_session_agents_dir(session.id, None)
        .await
        .unwrap();
    assert!(
        cursor
            .has_session_agents_dir_discovery(session.id)
            .await
            .unwrap()
    );
    assert_eq!(
        cursor.get_session_agents_dir(session.id).await.unwrap(),
        None
    );
    let project = ConfigGroup::AgentsDir {
        path: "/work/.agents".into(),
        probe_id,
    };
    cursor
        .set_session_agents_dir(session.id, Some(project.clone()))
        .await
        .unwrap();
    assert_eq!(
        cursor.get_session_agents_dir(session.id).await.unwrap(),
        Some(project)
    );
    cursor.clear_session_agents_dir(session.id).await.unwrap();
    assert!(
        !cursor
            .has_session_agents_dir_discovery(session.id)
            .await
            .unwrap()
    );
    assert_eq!(
        cursor.get_session_agents_dir(session.id).await.unwrap(),
        None
    );
    cursor.commit().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn unchanged_leaf_reused_and_pinned_creation_does_not_advance_edges() {
    let (_dir, db) = database().await;
    let mut cursor = db.cursor().await.unwrap();
    cursor
        .upsert_agent_configs(vec![config("a", &["b"]), config("b", &[])])
        .await
        .unwrap();
    let first = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    let mut changed = config("a", &["b"]);
    changed.description = "v2".into();
    cursor.upsert_agent_configs(vec![changed]).await.unwrap();
    let second = cursor
        .resolve_agent_config(&id("a"), &HashMap::new(), &HashMap::new())
        .await
        .unwrap();
    assert_ne!(first.definition.id, second.definition.id);
    assert_eq!(first.references.subagents, second.references.subagents);
    let selected = AgentModel::from(llm_api::providers::ModelConfig {
        model: llm_api::providers::Model::new(llm_api::providers::ProviderType::Openrouter, "test")
            .unwrap(),
        effort: llm_api::providers::Effort::Low,
    });
    let pinned = cursor
        .resolve_pinned_agent_config(first.definition.id, selected.clone(), &HashMap::new())
        .await
        .unwrap();
    assert_eq!(pinned.definition.description, "a");
    assert_eq!(pinned.references.subagents, first.references.subagents);
    assert_eq!(pinned.definition.model, selected);
    assert_eq!(
        pinned.definition.id,
        cursor
            .resolve_pinned_agent_config(first.definition.id, selected, &HashMap::new())
            .await
            .unwrap()
            .definition
            .id
    );
    cursor.commit().await.unwrap();
    db.close().await;
}
