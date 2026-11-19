use super::*;

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

fn lower_config(text: &str, defaults: &AgentDefaults) -> AgentConfig {
    let config: AgentDefinitionConfig = toml::from_str(text).unwrap();
    config
        .lower(
            &ConfigGroup::Global,
            Vec::new(),
            &HashSet::new(),
            &HashSet::new(),
            defaults,
        )
        .unwrap()
}

#[test]
fn omitted_components_inherit_configured_defaults() {
    let defaults = configured_defaults();
    let registry =
        ConfigRegistry::new(PathBuf::from("/tmp/defaults")).with_agent_defaults(defaults.clone());
    let config = lower_config("name = 'example'", registry.agent_defaults());
    assert_eq!(config.bundles, defaults.bundles);
}

#[test]
fn explicit_empty_components_disable_configured_defaults() {
    let defaults = configured_defaults();
    let config = lower_config("name = 'example'\nbundles = []", &defaults);
    assert!(config.bundles.is_empty());
}

#[test]
fn parameterized_bundle_configuration_is_parsed_from_toml() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ Startup = { cwd = false } }, { ExtraSystemPrompt = { prompt = 'First prompt' } }, 'StopWithoutTools']",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Startup {
                base_prompt: true,
                project_context: true,
                cwd: false,
                skills: true,
            },
            BundleConfig::ExtraSystemPrompt {
                prompt: "First prompt".into()
            },
            BundleConfig::StopWithoutTools,
        ]
    );
    for invalid in [
        "extra_system_prompt = 'legacy agent field'",
        "[hooks]\nagent_create = ['DefaultStartup']",
        "bundles = ['ExtraSystemPrompt']",
        "bundles = [{ ExtraSystemPrompt = {} }]",
        "bundles = [{ ExtraSystemPrompt = { prompt = 'text', unknown = true } }]",
        "bundles = ['Startup']",
        "tools = []",
        "tools = ['Bash']",
        "tools = ['CreateAgent']",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!("name = 'example'\n{invalid}"))
                .is_err(),
            "accepted invalid component config: {invalid}"
        );
    }
}

#[test]
fn bundle_flag_payloads_parse_from_toml_and_default_to_interactive_only() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ Goal = { interactive_only = false } }, { AskQuestion = {} }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Goal {
                interactive_only: false
            },
            BundleConfig::AskQuestion {
                interactive_only: true
            }
        ]
    );
    let config = lower_config(
        "name = 'example'\nbundles = [{ Goal = {} }, { AskQuestion = { interactive_only = false } }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::Goal {
                interactive_only: true
            },
            BundleConfig::AskQuestion {
                interactive_only: false
            }
        ]
    );
    for bundles in [
        "['Goal']",
        "['AskQuestion']",
        "[{ Goal = { unknown = true } }]",
        "[{ AskQuestion = { interactive_only = 'yes' } }]",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!(
                "name = 'example'\nbundles = {bundles}"
            ))
            .is_err()
        );
    }
}

#[test]
fn file_tools_payloads_parse_read_only_and_noop_bundle_selection() {
    let config = lower_config(
        "name = 'example'\nbundles = [{ FileTools = {} }, 'Noop']",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![
            BundleConfig::FileTools { read_only: false },
            BundleConfig::Noop
        ]
    );
    let config = lower_config(
        "name = 'example'\nbundles = [{ FileTools = { read_only = true } }]",
        &AgentDefaults::default(),
    );
    assert_eq!(
        config.bundles,
        vec![BundleConfig::FileTools { read_only: true }]
    );
    for bundles in [
        "['FileTools']",
        "[{ FileTools = { read_only = 'yes' } }]",
        "[{ FileTools = { unknown = true } }]",
    ] {
        assert!(
            toml::from_str::<AgentDefinitionConfig>(&format!(
                "name = 'example'\nbundles = {bundles}"
            ))
            .is_err()
        );
    }
}

#[test]
fn file_defaults_and_explicit_values() {
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'").unwrap();
    assert!(config.first_model_default);
    assert_eq!(config.models, vec![ModelSelection::Global]);
    assert!(config.skills.global_defaults && config.skills.group_defaults);
    assert!(config.subagents.global_defaults && config.subagents.group_defaults);
    let config: AgentDefinitionConfig = toml::from_str("name = 'example'\ndefault = false\nmodels = []\n[skills]\nglobal_defaults = false\n[subagents]\ngroup_defaults = false").unwrap();
    assert!(!config.first_model_default);
    assert!(config.models.is_empty());
    assert!(!config.skills.global_defaults && config.skills.group_defaults);
    assert!(config.subagents.global_defaults && !config.subagents.group_defaults);
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
    service.refresh(&database, &[]).await.unwrap();
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
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.rollback().await.unwrap();

    // A syntax error must not publish deletions or partially replace good rows.
    tokio::fs::write(global.join("agents/example.toml"), "invalid = [")
        .await
        .unwrap();
    assert!(service.refresh(&database, &[]).await.is_err());
    let mut cursor = database.cursor().await.unwrap();
    assert_eq!(cursor.list_agent_configs().await.unwrap().len(), 1);
    assert_eq!(cursor.list_skills().await.unwrap()[0].id, skills[0].id);
    cursor.rollback().await.unwrap();
    // A successful empty source removes only discovery-owned records.
    tokio::fs::remove_file(global.join("agents/example.toml"))
        .await
        .unwrap();
    service.refresh(&database, &[]).await.unwrap();
    let mut cursor = database.cursor().await.unwrap();
    assert!(cursor.list_agent_configs().await.unwrap().is_empty());
    assert!(cursor.list_skills().await.unwrap().is_empty());
    cursor.rollback().await.unwrap();
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
    let mut cursor = db.cursor().await.unwrap();
    assert!(cursor.list_agent_configs().await.unwrap().is_empty());
    cursor.rollback().await.unwrap();

    // An explicit list replaces the defaults, including their StopWithoutTools.
    tokio::fs::write(&path, format!("name = 'goal'\nbundles = [{goal}]\n"))
        .await
        .unwrap();
    registry.refresh(&db, &[]).await.unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let configs = cursor.list_agent_configs().await.unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(
        configs[0].config.bundles,
        vec![BundleConfig::Goal {
            interactive_only: false
        }]
    );
    cursor.rollback().await.unwrap();
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

#[test]
fn global_group_defaults_include_the_session_group() {
    let available = HashSet::new();
    let global = expand_inclusions(true, true, &[], &ConfigGroup::Global, &available);
    assert_eq!(
        global.groups,
        vec![ConfigGroup::Global, ConfigGroup::Session]
    );
    let global_only = expand_inclusions(true, false, &[], &ConfigGroup::Global, &available);
    assert_eq!(global_only.groups, vec![ConfigGroup::Global]);

    let project = ConfigGroup::Custom("project".into());
    let own = expand_inclusions(true, true, &[], &project, &available);
    assert_eq!(own.groups, vec![ConfigGroup::Global, project]);
}
