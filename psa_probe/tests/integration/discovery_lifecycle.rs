use super::*;
use psa_api::{CreateSession, ForkPoint, ForkSession, NewConfiguredAgent, UpdateSession};

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
        tar_data: archive.into_inner().unwrap(),
    }
}

#[tokio::test]
async fn configured_creation_refreshes_sources_uploads_and_retains_selected_project() {
    let harness = Harness::start().await;
    let global = harness.dir.path().join("global");
    let project = harness.dir.path().join("work");
    let nested = project.join("nested");
    for directory in [
        global.join("agents"),
        global.join("skills/global"),
        project.join(".agents/skills/project"),
        nested.clone(),
    ] {
        tokio::fs::create_dir_all(directory).await.unwrap();
    }
    tokio::fs::write(
        global.join("agents/review.toml"),
        "name = 'review'\n[hooks]\nagent_create = ['Skills']\n",
    )
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
    let session = harness.client.create_session(&CreateSession {
        name: "configured".into(), working_directory: nested.clone(), probe_id: harness.id,
        model_config: Some(serde_json::from_value(serde_json::json!({"model":{"provider":"Openrouter","api_name":"test-model"}, "effort":"Medium"})).unwrap()),
    }).await.unwrap();
    // Introducing a nearer project must not change the project's retained identity.
    tokio::fs::create_dir_all(nested.join(".agents"))
        .await
        .unwrap();
    let request = NewConfiguredAgent {
        config_id: ConfigId {
            config_group: ConfigGroup::Global,
            name: "review".into(),
        },
        name: "review".into(),
        subagent_parent_id: None,
        interactive: false,
        model: Some(psa_api::ModelSelection::Global),
    };
    let listed = harness
        .client
        .refresh_and_list_skills(Some(session.id))
        .await
        .unwrap();
    assert_eq!(listed.len(), 3);
    assert!(!harness.metadata.data_dir.join("skill-cache").exists());
    let agent = harness
        .client
        .create_agent(session.id, &request)
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(agent.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.references.skills.len(), 3);
    assert!(
        snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );
    let uploaded_path = harness
        .metadata
        .data_dir
        .join("skill-cache")
        .join(first_upload.id.0.to_string());
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
    assert!(!uploaded_path.join("old.txt").exists());
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
        .unwrap()
        .unwrap();
    assert!(next_snapshot.references.skills.contains(&first_upload.id));

    // Fork without a cwd override retains the selected project even though the
    // current cwd now has a newly introduced, nearer empty .agents directory.
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
        .unwrap()
        .unwrap();
    assert!(
        fork_snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );

    // An actual cwd change rediscovers that empty nearest project and excludes
    // the ancestor skills, rather than merging ancestor projects.
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
    let agent = harness
        .client
        .create_agent(session.id, &request)
        .await
        .unwrap();
    let snapshot = harness
        .client
        .get_resolved_agent_config(agent.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.references.skills.len(), 2);
    assert!(
        !snapshot
            .definition
            .skills
            .contains(&project.join(".agents/skills/project/SKILL.md"))
    );
    harness.stop().await;
}
