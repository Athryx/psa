use super::*;

#[test]
fn file_defaults_and_explicit_values() {
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'").unwrap();
    assert!(config.default);
    assert_eq!(config.models, vec![ModelSelection::Global]);
    assert!(config.skills.global_defaults && config.skills.project_defaults);
    assert!(config.subagents.global_defaults && config.subagents.project_defaults);
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'\ndefault = false\nmodels = []\n[skills]\nglobal_defaults = false\n[subagents]\nproject_defaults = false").unwrap();
    assert!(!config.default);
    assert!(config.models.is_empty());
    assert!(!config.skills.global_defaults && config.skills.project_defaults);
    assert!(config.subagents.global_defaults && !config.subagents.project_defaults);
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
    service.refresh(&database, None).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
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
    cursor.rollback().await.unwrap();

    // Deleted additional_paths retain their metadata-derived reference and
    // become missing components; restoration reuses the mutable skill identity.
    let skill_file = global.join("extra/fallback/SKILL.md");
    let contents = tokio::fs::read(&skill_file).await.unwrap();
    tokio::fs::remove_file(&skill_file).await.unwrap();
    service.refresh(&database, None).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
    assert!(cursor.list_skills().await.unwrap().is_empty());
    let missing = cursor.list_agent_configs().await.unwrap();
    assert_eq!(
        missing[0].config.skills.items,
        vec![skills[0].config_id.clone()]
    );
    assert_eq!(missing[0].status.missing_components.len(), 1);
    cursor.rollback().await.unwrap();
    tokio::fs::write(&skill_file, &contents).await.unwrap();
    service.refresh(&database, None).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.rollback().await.unwrap();

    // A syntax error must not publish deletions or partially replace good rows.
    tokio::fs::write(global.join("agents/example.toml"), "invalid = [")
        .await
        .unwrap();
    assert!(service.refresh(&database, None).await.is_err());
    let mut cursor = database.cursor().await.unwrap();
    assert_eq!(cursor.list_agent_configs().await.unwrap().len(), 1);
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.rollback().await.unwrap();
    // A successful empty source removes only discovery-owned records.
    tokio::fs::remove_file(global.join("agents/example.toml"))
        .await
        .unwrap();
    service.refresh(&database, None).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
    assert!(cursor.list_agent_configs().await.unwrap().is_empty());
    assert!(cursor.list_skills().await.unwrap().is_empty());
    cursor.rollback().await.unwrap();
    database.close().await;
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
