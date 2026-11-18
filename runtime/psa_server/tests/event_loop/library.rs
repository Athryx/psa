//! The high-level `psa` layer against a real server: contexts, specs, runs,
//! and event ranges.
use super::*;
use psa::{
    AgentLayout, AgentSpec, Context, EventRange, Inclusions, ProbeSelector, SessionSpec, Skill,
};
use wire::{AgentResult, BundleConfig, HookConfig, HookOverrides};

fn worker_layout() -> AgentLayout {
    AgentLayout::new("worker")
        .bundles(vec![BundleConfig::Noop])
        .hooks(HookOverrides {
            agent_create: Some(vec![
                HookConfig::DefaultStartup,
                HookConfig::ToolDefinitions,
            ]),
            after_request: Some(vec![HookConfig::StopWithoutTools]),
            ..Default::default()
        })
}

#[tokio::test]
async fn contexts_create_sessions_and_agents_from_specs_and_run_them() {
    let harness = Harness::start(vec![]).await;
    let context = Context::builder(harness.client.clone(), "library")
        .probe(ProbeSelector::Hostname("test".into()))
        .working_directory("/work")
        .build()
        .await
        .unwrap();
    assert_eq!(
        context.config_group(),
        &wire::ConfigGroup::Custom("library".into())
    );

    let session = context
        .ensure_session(SessionSpec::new("triage").model(model()))
        .await
        .unwrap();
    let again = context
        .ensure_session(SessionSpec::new("triage"))
        .await
        .unwrap();
    assert_eq!(again.id(), session.id());
    assert_eq!(
        session.info().project_id,
        Some(wire::ProjectId("library".into()))
    );

    // The session runs on the restricted plain probe, which is already live.
    let probes = context.probes().await.unwrap();
    assert_eq!(probes.len(), 1);
    assert!(probes[0].is_live() && !probes[0].is_sandbox());
    let probe = session.ensure_probe_up().await.unwrap();
    assert_eq!(probe.id(), session.info().probe_id);
    session.stop_probe().await.unwrap();
    assert!(session.probe().await.unwrap().is_live());

    let agent = session
        .ensure_agent(AgentSpec::new("worker", worker_layout()))
        .await
        .unwrap();
    let same = session
        .ensure_agent(AgentSpec::new("worker", worker_layout()))
        .await
        .unwrap();
    assert_eq!(same.id(), agent.id());

    let outcome = tokio::time::timeout(Duration::from_secs(15), agent.run("hi"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.state, AgentState::Idle);
    assert_eq!(
        outcome.result.unwrap().value,
        AgentResult::Text("hello".into())
    );
    let text: Option<String> = agent.last_result_as().await.unwrap();
    assert_eq!(text.as_deref(), Some("hello"));

    // Named configs resolve in the context's group; unknown names are errors.
    let named = session
        .create_agent(AgentSpec::new("second", "worker"))
        .await
        .unwrap();
    assert_eq!(named.name(), "second");
    let missing = session
        .create_agent(AgentSpec::new("third", "missing"))
        .await;
    assert!(matches!(missing, Err(psa::Error::UnknownConfig(name)) if name == "missing"));

    harness.close().await;
}

#[tokio::test]
async fn layout_trees_register_inline_layouts_and_skills_once() {
    let harness = Harness::start(vec![]).await;
    let context = Context::builder(harness.client.clone(), "library")
        .build()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("checklist");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        "---\ndescription: A checklist\n---\nCheck things.",
    )
    .unwrap();
    let skill = Skill::from_path(&source).await.unwrap();

    let reviewer = AgentLayout::new("reviewer")
        .description("Reviews")
        .skills(Inclusions::default().include(skill.clone()));
    let layout = AgentLayout::new("lead")
        .subagents(Inclusions::default().include(reviewer).include("lead"))
        .skills(Inclusions::default().include(skill.clone()));
    let lead = context.register_layout(&layout).await.unwrap();

    let group = context.config_group().clone();
    let configs = harness.client.list_agent_configs().await.unwrap();
    let config = |name: &str| {
        configs
            .iter()
            .find(|status| {
                status.config.config_id.config_group == group
                    && status.config.config_id.name == name
            })
            .map(|status| status.config.clone())
            .unwrap()
    };
    let reviewer_id = config("reviewer").config_id;
    assert_eq!(config("lead").config_id, lead);
    assert_eq!(
        config("lead").subagents.items,
        vec![reviewer_id, lead.clone()]
    );
    assert_eq!(config("reviewer").skills.items[0].name, "checklist");

    let uploaded = |skills: Vec<wire::Skill>| {
        skills
            .into_iter()
            .find(|registered| registered.config_id.config_group == group)
            .unwrap()
    };
    let first = uploaded(harness.client.list_skills().await.unwrap());
    assert_eq!(first.name, "checklist");
    assert_eq!(
        first.archive_sha256.as_deref(),
        Some(skill.archive_sha256())
    );

    // An unchanged archive is not uploaded again.
    context.register_layout(&layout).await.unwrap();
    let second = uploaded(harness.client.list_skills().await.unwrap());
    assert_eq!(second.skill_location, first.skill_location);

    std::fs::write(source.join("extra.txt"), "more").unwrap();
    let changed = Skill::from_path(&source).await.unwrap();
    context.register_skill(&changed).await.unwrap();
    let third = uploaded(harness.client.list_skills().await.unwrap());
    assert_ne!(third.skill_location, first.skill_location);
    assert_eq!(
        third.archive_sha256.as_deref(),
        Some(changed.archive_sha256())
    );

    harness.close().await;
}

#[tokio::test]
async fn event_ranges_resolve_bounds_against_agent_and_session_transcripts() {
    let harness = Harness::start(vec![]).await;
    let context = Context::builder(harness.client.clone(), "ranges")
        .probe(ProbeSelector::Hostname("test".into()))
        .working_directory("/work")
        .build()
        .await
        .unwrap();
    let session = context
        .create_session(SessionSpec::new("ranges").model(model()))
        .await
        .unwrap();
    let agent = session
        .create_agent(AgentSpec::new("worker", worker_layout()))
        .await
        .unwrap();
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(15), agent.run("hi"))
            .await
            .unwrap()
            .unwrap();
    }

    let ids = |events: &[wire::EventEnvelope]| -> Vec<u64> {
        events.iter().map(|event| event.event.id.0).collect()
    };
    let all = agent.events(EventRange::all()).await.unwrap();
    assert!(all.len() > 3);

    let last = agent.events(EventRange::last(3)).await.unwrap();
    assert_eq!(ids(&last), ids(&all[all.len() - 3..]));

    let middle = all[all.len() / 2].event.id.0;
    let through = agent
        .events(EventRange::all().ending(psa::EventEnd::Through(middle)))
        .await
        .unwrap();
    assert_eq!(ids(&through), ids(&all[..=all.len() / 2]));
    let at = agent
        .events(EventRange::all().starting(psa::EventStart::At(middle)))
        .await
        .unwrap();
    assert_eq!(ids(&at), ids(&all[all.len() / 2..]));

    let results = agent
        .events(EventRange::all().results_only())
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    let last_result = agent
        .events(EventRange::last(1).results_only())
        .await
        .unwrap();
    assert_eq!(ids(&last_result), ids(&results[1..]));

    let context_events = agent.events(EventRange::current_context()).await.unwrap();
    assert!(!context_events.is_empty());
    assert_eq!(
        context_events.last().unwrap().event.id,
        all.last().unwrap().event.id
    );

    let session_events = session.events(EventRange::all()).await.unwrap();
    assert!(session_events.len() >= all.len());
    assert!(matches!(
        session.events(EventRange::current_context()).await,
        Err(psa::Error::InvalidRange(_))
    ));
    assert!(matches!(
        agent.events(EventRange::follow()).await,
        Err(psa::Error::InvalidRange(_))
    ));

    harness.close().await;
}
