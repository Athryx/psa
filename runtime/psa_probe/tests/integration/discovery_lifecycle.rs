use super::*;
use psa::api::{CreateAgent, CreateSession, ForkPoint, ForkSession, UpdateSession};

fn upload(version: &str) -> NewSkill {
    let mut archive = tar::Builder::new(Vec::new());
    for (path, bytes) in [
        (
            "SKILL.md",
            "---\nname: uploaded\ndescription: uploaded skill\n---\n",
        ),
        (version, version),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, path, bytes.as_bytes())
            .unwrap();
    }
    NewSkill {
        config_id: ConfigId {
            config_group: ConfigGroup::Global,
            name: "uploaded".into(),
        },
        archive: psa::api::Archive {
            tar_data: archive.into_inner().unwrap(),
            compression: Default::default(),
        },
    }
}

#[tokio::test]
async fn configured_creation_refreshes_sources_uploads_and_retains_selected_agents_dir() {
    assert_configured_creation(false).await;
}

#[tokio::test]
async fn host_configured_creation_uses_original_skill_sources_without_cache() {
    assert_configured_creation(true).await;
}

async fn assert_configured_creation(host: bool) {
    let harness = Harness::start_with_host(host).await;
    assert_eq!(harness.metadata.host, host);
    assert_eq!(
        harness
            .client
            .get_probe(harness.id)
            .await
            .unwrap()
            .metadata
            .host,
        host
    );
    let global = harness.dir.path().join("global");
    let project = harness.dir.path().join("work");
    let nested = project.join("nested");
    for directory in [
        global.join("skills/global"),
        project.join(".agents/agents"),
        project.join(".agents/skills/project"),
        nested.clone(),
    ] {
        tokio::fs::create_dir_all(directory).await.unwrap();
    }
    // Default inclusion takes Global plus the agent's own `.agents` group.
    let review_toml =
        "name = 'review'\nbundles = [{ Startup = { project_context = false, cwd = false } }]\n";
    tokio::fs::write(project.join(".agents/agents/review.toml"), review_toml)
        .await
        .unwrap();
    tokio::fs::write(
        global.join("skills/global/SKILL.md"),
        "---\ndescription: global skill\n---\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        project.join(".agents/skills/project/SKILL.md"),
        "---\ndescription: project skill\n---\n",
    )
    .await
    .unwrap();
    let first_upload = harness
        .client
        .upload_skill(&upload("old.txt"))
        .await
        .unwrap();
    let session = harness
        .client
        .create_session(&CreateSession {
            decision_model: None,
            name: "configured".into(),
            working_directory: nested.clone(),
            probe_id: harness.id,
            project_id: None,
            model_config: Some(
                serde_json::from_value(
                    serde_json::json!({"model":"openrouter/test-model", "effort":"Medium"}),
                )
                .unwrap(),
            ),
        })
        .await
        .unwrap();
    // Introducing a nearer `.agents` directory must not change the session's
    // retained selection.
    tokio::fs::create_dir_all(nested.join(".agents/agents"))
        .await
        .unwrap();
    tokio::fs::write(nested.join(".agents/agents/review.toml"), review_toml)
        .await
        .unwrap();
    let agents_dir = |path: &std::path::Path| ConfigGroup::AgentsDir {
        path: path.join(".agents").to_str().unwrap().into(),
        probe_id: harness.id,
    };
    let request = CreateAgent {
        config_id: ConfigId {
            config_group: agents_dir(&project),
            name: "review".into(),
        },
        name: "review".into(),
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        model: Some(psa::api::ModelSelection::Global),
    };
    harness.client.refresh_configs(&[session.id]).await.unwrap();
    let listed = harness.client.list_skills().await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(
        !tokio::fs::try_exists(harness.metadata.data_dir.join("skill-cache"))
            .await
            .unwrap()
    );
    let agent = harness
        .client
        .create_agent(session.id, &request)
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(agent.definition_id)
        .await
        .unwrap();
    assert_eq!(snapshot.references.skills.len(), 3);
    assert!(
        snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );
    let cache = harness.metadata.data_dir.join("skill-cache");
    let SkillLocation::Global(uploaded_source) = &first_upload.skill_location else {
        panic!("uploaded skills must have a server-owned source");
    };
    let uploaded_path = if host {
        assert!(!tokio::fs::try_exists(&cache).await.unwrap());
        assert!(
            snapshot
                .definition
                .skills
                .contains(&global.join("skills/global/SKILL.md"))
        );
        uploaded_source.clone()
    } else {
        cache.join(first_upload.id.0.to_string())
    };
    assert!(
        snapshot
            .definition
            .skills
            .contains(&uploaded_path.join("SKILL.md"))
    );
    assert_eq!(
        tokio::fs::read_to_string(uploaded_path.join("old.txt"))
            .await
            .unwrap(),
        "old.txt"
    );

    let updated = harness
        .client
        .upload_skill(&upload("new.txt"))
        .await
        .unwrap();
    assert_eq!(first_upload.id, updated.id);
    let next = harness
        .client
        .create_agent(session.id, &request)
        .await
        .unwrap();
    assert!(
        !tokio::fs::try_exists(uploaded_path.join("old.txt"))
            .await
            .unwrap()
    );
    let uploaded_path = if host {
        let SkillLocation::Global(source) = &updated.skill_location else {
            panic!("uploaded skills must have a server-owned source");
        };
        assert_ne!(*source, uploaded_path);
        source.clone()
    } else {
        uploaded_path
    };
    assert_eq!(
        tokio::fs::read_to_string(uploaded_path.join("new.txt"))
            .await
            .unwrap(),
        "new.txt"
    );
    let next_snapshot = harness
        .client
        .get_resolved_agent_config(next.definition_id)
        .await
        .unwrap();
    assert!(next_snapshot.references.skills.contains(&first_upload.id));
    assert!(
        next_snapshot
            .definition
            .skills
            .contains(&uploaded_path.join("SKILL.md"))
    );
    if host {
        assert!(!tokio::fs::try_exists(&cache).await.unwrap());
    }

    // Fork without a cwd override retains the selected directory even though the
    // current cwd now has a newly introduced, nearer .agents directory.
    let forked = harness
        .client
        .fork_session(
            session.id,
            &ForkSession {
                point: ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let fork_agent = harness
        .client
        .create_agent(forked.session.id, &request)
        .await
        .unwrap();
    let fork_snapshot = harness
        .client
        .get_resolved_agent_config(fork_agent.definition_id)
        .await
        .unwrap();
    assert!(
        fork_snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );

    // An actual cwd change rediscovers the nearest directory; its agent sees
    // only Global and its own (skill-less) group, never the ancestor's skills.
    let deeper = nested.join("deeper");
    tokio::fs::create_dir_all(&deeper).await.unwrap();
    harness
        .client
        .update_session(
            session.id,
            &UpdateSession {
                working_directory: Some(deeper),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let nearer = CreateAgent {
        config_id: ConfigId {
            config_group: agents_dir(&nested),
            name: "review".into(),
        },
        ..request.clone()
    };
    let agent = harness
        .client
        .create_agent(session.id, &nearer)
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(agent.definition_id)
        .await
        .unwrap();
    assert_eq!(snapshot.references.skills.len(), 2);
    if host {
        assert!(!tokio::fs::try_exists(&cache).await.unwrap());
    }
    assert!(
        !snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );
    harness.stop().await;
}
