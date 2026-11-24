use super::*;
use psa::api::{BundleConfig, ModelSelection};

fn configured_defaults() -> AgentDefaults {
    AgentDefaults {
        bundles: vec![
            BundleConfig::Startup {
                base_prompt: true,
                project_context: false,
                cwd: false,
                skills: true,
            },
            BundleConfig::FileTools { read_only: true },
            BundleConfig::StopWithoutTools,
        ],
    }
}

#[tokio::test]
async fn relative_additional_paths_metadata_and_failed_scans_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let global = directory.path().join("global");
    tokio::fs::create_dir_all(global.join("agents"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(global.join("extra/fallback"))
        .await
        .unwrap();
    tokio::fs::write(
        global.join("extra/fallback/SKILL.md"),
        "---\nname: actual\ndescription: useful\ndisable-model-invocation: true\n---\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        global.join("agents/example.toml"),
        "name = 'example'\n[skills]\nadditional_paths = ['../extra/fallback']\n",
    )
    .await
    .unwrap();
    let database = Database::open(directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let service = ConfigRegistry::new(global.clone());
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.read().await.unwrap();
    let skills = cursor.list_skills().await.unwrap();
    let configs = cursor.list_agent_configs().await.unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "actual");
    assert!(skills[0].disable_model_invocation);
    assert_eq!(configs.len(), 1);
    assert_eq!(
        configs[0].config.skills.items,
        vec![skills[0].config_id.clone()]
    );
    assert_eq!(configs[0].config.models, vec![ModelSelection::Global]);
    cursor.close().await.unwrap();

    // Deleted additional_paths retain their metadata-derived reference and
    // become missing components; restoration reuses the mutable skill identity.
    let skill_file = global.join("extra/fallback/SKILL.md");
    let contents = tokio::fs::read(&skill_file).await.unwrap();
    tokio::fs::remove_file(&skill_file).await.unwrap();
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.read().await.unwrap();
    assert!(cursor.list_skills().await.unwrap().is_empty());
    let missing = cursor.list_agent_configs().await.unwrap();
    assert_eq!(
        missing[0].config.skills.items,
        vec![skills[0].config_id.clone()]
    );
    assert_eq!(missing[0].status.missing_components.len(), 1);
    cursor.close().await.unwrap();
    tokio::fs::write(&skill_file, &contents).await.unwrap();
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.read().await.unwrap();
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.close().await.unwrap();

    // A syntax error must not publish deletions or partially replace good rows.
    tokio::fs::write(global.join("agents/example.toml"), "invalid = [")
        .await
        .unwrap();
    assert!(service.refresh(&database, &[]).await.is_err());
    let mut cursor = database.read().await.unwrap();
    assert_eq!(cursor.list_agent_configs().await.unwrap().len(), 1);
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.close().await.unwrap();
    // A successful empty source removes only discovery-owned records.
    tokio::fs::remove_file(global.join("agents/example.toml"))
        .await
        .unwrap();
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.read().await.unwrap();
    assert!(cursor.list_agent_configs().await.unwrap().is_empty());
    assert!(cursor.list_skills().await.unwrap().is_empty());
    cursor.close().await.unwrap();
    database.close().await;
}

#[tokio::test]
async fn goal_source_must_not_also_select_ordinary_completion() {
    let directory = tempfile::tempdir().unwrap();
    let global = directory.path().join("global");
    tokio::fs::create_dir_all(global.join("agents"))
        .await
        .unwrap();
    let path = global.join("agents/goal.toml");
    let goal = "{ Goal = { interactive_only = false } }";
    tokio::fs::write(
        &path,
        format!("name = 'goal'\nbundles = [{goal}, 'StopWithoutTools']\n"),
    )
    .await
    .unwrap();
    let db = Database::open(directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let registry = ConfigRegistry::new(global).with_agent_defaults(configured_defaults());
    assert!(registry.refresh(&db, &[]).await.is_err());
    let mut cursor = db.read().await.unwrap();
    assert!(cursor.list_agent_configs().await.unwrap().is_empty());
    cursor.close().await.unwrap();

    // An explicit list replaces the defaults, including their StopWithoutTools.
    tokio::fs::write(&path, format!("name = 'goal'\nbundles = [{goal}]\n"))
        .await
        .unwrap();
    registry.refresh(&db, &[]).await.unwrap();
    let mut cursor = db.read().await.unwrap();
    let configs = cursor.list_agent_configs().await.unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(
        configs[0].config.bundles,
        vec![BundleConfig::Goal {
            interactive_only: false
        }]
    );
    cursor.close().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn duplicate_metadata_names_and_empty_models_are_errors() {
    let directory = tempfile::tempdir().unwrap();
    for name in ["first", "second"] {
        let skill = directory.path().join("skills").join(name);
        tokio::fs::create_dir_all(&skill).await.unwrap();
        tokio::fs::write(
            skill.join("SKILL.md"),
            "---\nname: duplicate\ndescription: text\n---\n",
        )
        .await
        .unwrap();
    }
    assert!(
        scan_group(
            Source::Local,
            directory.path().to_owned(),
            ConfigGroup::Global,
            &[]
        )
        .await
        .is_err()
    );
    tokio::fs::remove_dir_all(directory.path().join("skills"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(directory.path().join("agents"))
        .await
        .unwrap();
    tokio::fs::write(
        directory.path().join("agents/example.toml"),
        "name='example'\nmodels=[]",
    )
    .await
    .unwrap();
    assert!(
        scan_group(
            Source::Local,
            directory.path().to_owned(),
            ConfigGroup::Global,
            &[]
        )
        .await
        .is_err()
    );
}
