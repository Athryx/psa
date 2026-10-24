use super::*;
use psa_api::{ConfigGroup, ConfigId, NewSkill, Skill, SkillId, SkillLocation};
use psa_server::config_registry::ConfigRegistry;

#[path = "discovery_lifecycle.rs"]
mod lifecycle;

#[tokio::test]
async fn nearest_empty_project_stops_ancestor_discovery() {
    let harness = Harness::start().await;
    let root = harness.dir.path().join("project");
    let inner = root.join("inner");
    tokio::fs::create_dir_all(root.join(".agents/skills"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(inner.join(".agents"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(inner.join("nested"))
        .await
        .unwrap();
    let service = ConfigRegistry::new(harness.dir.path().join("global"));
    let probe = harness.handle().await;
    let selected = service
        .discover_project(&probe, harness.id, &inner.join("nested"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        selected,
        ConfigGroup::Project {
            path: inner.join(".agents").to_string_lossy().into_owned(),
            probe_id: harness.id
        }
    );
    tokio::fs::remove_dir(inner.join(".agents")).await.unwrap();
    let selected = service
        .discover_project(&probe, harness.id, &inner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        selected,
        ConfigGroup::Project {
            path: root.join(".agents").to_string_lossy().into_owned(),
            probe_id: harness.id
        }
    );
    harness.stop().await;
}

#[tokio::test]
async fn global_skills_sync_all_content_and_probe_skills_remain_in_place() {
    let harness = Harness::start().await;
    let global = harness.dir.path().join("global");
    let source = global.join("skills/example");
    tokio::fs::create_dir_all(&source).await.unwrap();
    tokio::fs::write(
        source.join("SKILL.md"),
        "---\nname: example\ndescription: useful\n---\n",
    )
    .await
    .unwrap();
    tokio::fs::write(source.join("script.py"), "old")
        .await
        .unwrap();
    let service = ConfigRegistry::new(global);
    let probe = harness.handle().await;
    let skill = Skill {
        id: SkillId(123),
        config_id: ConfigId {
            config_group: ConfigGroup::Global,
            name: "example".into(),
        },
        name: "example".into(),
        description: "useful".into(),
        skill_location: SkillLocation::Global(source.clone()),
        disable_model_invocation: false,
    };
    let paths = service
        .materialize_skills(
            &probe,
            harness.id,
            &harness.metadata.data_dir,
            &[skill.clone()],
        )
        .await
        .unwrap();
    let materialized = paths.get(&skill.id).unwrap().clone();
    assert_ne!(materialized, source.join("SKILL.md"));
    assert_eq!(
        tokio::fs::read_to_string(materialized.parent().unwrap().join("script.py"))
            .await
            .unwrap(),
        "old"
    );
    tokio::fs::remove_file(source.join("script.py"))
        .await
        .unwrap();
    tokio::fs::write(source.join("helper.txt"), "new")
        .await
        .unwrap();
    let updated = service
        .materialize_skills(
            &probe,
            harness.id,
            &harness.metadata.data_dir,
            &[skill.clone()],
        )
        .await
        .unwrap();
    assert_eq!(updated[&skill.id], materialized);
    assert!(!materialized.parent().unwrap().join("script.py").exists());
    assert_eq!(
        tokio::fs::read_to_string(materialized.parent().unwrap().join("helper.txt"))
            .await
            .unwrap(),
        "new"
    );
    let local = Skill {
        skill_location: SkillLocation::Probe {
            id: harness.id,
            path: source.clone(),
        },
        ..skill.clone()
    };
    let paths = service
        .materialize_skills(&probe, harness.id, &harness.metadata.data_dir, &[local])
        .await
        .unwrap();
    assert_eq!(paths[&skill.id], source.join("SKILL.md"));
    let wrong = Skill {
        skill_location: SkillLocation::Probe {
            id: ProbeId(Uuid::new_v4()),
            path: source,
        },
        ..skill
    };
    assert!(
        service
            .materialize_skills(&probe, harness.id, &harness.metadata.data_dir, &[wrong])
            .await
            .is_err()
    );
    harness.stop().await;
}
