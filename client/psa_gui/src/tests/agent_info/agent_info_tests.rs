use super::*;
use client_shared::DesktopSettings;
use dioxus::core::Mutation;
use serde_json::json;
use std::rc::Rc;

fn resolved_config() -> ResolvedAgentConfig {
    let bundles = json!([
        {"Startup": {"base_prompt": true, "project_context": true, "cwd": true, "skills": false}},
        {"ExtraSystemPrompt": {"prompt": "Be concise.\nAlways cite files."}},
        {"FileTools": {"read_only": false}},
        "StopWithoutTools",
    ]);
    serde_json::from_value(json!({
        "config": {
            "config_id": {"config_group": "Global", "name": "coder"},
            "description": "Writes code",
            "bundles": bundles,
            "models": ["Global"],
        },
        "definition": {
            "id": 1, "name": "coder", "description": "Writes code", "selectable": true,
            "bundles": bundles, "model": "Global", "skills": [], "permitted_subagents": [],
        },
        "references": {"subagents": [], "skills": [7]},
    }))
    .unwrap()
}

fn components(config: &ResolvedAgentConfig) -> AgentComponents {
    let mut bundles: Vec<_> = config
        .definition
        .bundles
        .iter()
        .map(|config| InstalledBundle {
            config: config.clone(),
            tools: Vec::new(),
            hooks: Vec::new(),
            codemode_functions: Vec::new(),
        })
        .collect();
    let hook = |stage, name: &str| InstalledHook {
        stage,
        name: name.into(),
    };
    bundles[0].hooks = vec![
        hook(HookStage::AgentCreate, "DefaultStartup"),
        hook(HookStage::AgentCreate, "ProjectContext"),
        hook(HookStage::SessionUpdate, "SessionModelUpdate"),
    ];
    bundles[2].tools = vec![InstalledTool {
        id: ToolId::Read,
        name: "read".into(),
        description: "Reads a file.\nSupports line ranges.".into(),
    }];
    bundles[3].hooks = vec![hook(HookStage::AfterRequest, "StopWithoutTools")];
    AgentComponents { bundles }
}

fn skill() -> Skill {
    serde_json::from_value(json!({
        "id": 7,
        "config_id": {"config_group": "Global", "name": "deploy"},
        "name": "deploy",
        "description": "Deploys the service",
        "skill_location": {"Global": "/skills/deploy"},
        "disable_model_invocation": true,
    }))
    .unwrap()
}

fn details() -> DefinitionDetails {
    let config = resolved_config();
    DefinitionDetails {
        components: Ok(components(&config)),
        config: Ok(config),
        skills: Ok(vec![skill()]),
        model_usage: Ok(Vec::new()),
        subagents: Vec::new(),
        context: None,
    }
}

fn render(render: fn(&DefinitionDetails) -> Element) -> String {
    thread_local! {
        static RENDER: std::cell::Cell<Option<fn(&DefinitionDetails) -> Element>> = const { std::cell::Cell::new(None) };
    }
    fn root() -> Element {
        let render = RENDER.with(|cell| cell.get()).unwrap();
        render(&details())
    }
    RENDER.with(|cell| cell.set(Some(render)));
    let mut dom = VirtualDom::new(root);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn bundle_parameters_are_labeled_fields_and_units_have_none() {
    let startup = BundleConfig::Startup {
        base_prompt: true,
        project_context: false,
        cwd: true,
        skills: true,
    };
    let labels: Vec<_> = bundle_parameters(&startup)
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert!(labels.contains(&"Base prompt".to_owned()), "{labels:?}");
    assert!(labels.contains(&"Project context".to_owned()), "{labels:?}");
    assert!(bundle_parameters(&BundleConfig::Bash).is_empty());
}

#[test]
fn bundles_tab_shows_parameters_and_what_each_bundle_installed() {
    let html = render(render_bundles);
    for expected in [
        "Startup",
        "Project context",
        "ExtraSystemPrompt",
        // Multi-line prompts collapse behind a summary.
        "2 lines",
        "Be concise.",
        "FileTools",
        "chip chip-tool",
        "read",
        "ProjectContext",
        "StopWithoutTools",
    ] {
        assert!(html.contains(expected), "missing {expected}: {html}");
    }
}

#[test]
fn hooks_tab_groups_hooks_by_stage_in_run_order() {
    let html = render(render_hooks);
    let create = html.find("Agent creation").unwrap();
    let after = html.find("After inference").unwrap();
    let startup = html.find("DefaultStartup").unwrap();
    let project = html.find("ProjectContext").unwrap();
    let stop = html.find(">StopWithoutTools<").unwrap();
    assert!(
        create < startup && startup < project && project < after,
        "{html}"
    );
    assert!(after < stop, "{html}");
    // Stages without hooks are still listed.
    assert!(html.contains("Command") && html.contains("None"), "{html}");
}

#[test]
fn tools_and_skills_tabs_list_descriptions() {
    let html = render(render_tools);
    assert!(
        html.contains("read") && html.contains("Supports line ranges."),
        "{html}"
    );
    assert!(html.contains("FileTools"), "{html}");

    let html = render(render_skills);
    assert!(
        html.contains("deploy") && html.contains("Deploys the service"),
        "{html}"
    );
    assert!(html.contains("manual only"), "{html}");
    // Startup in this definition leaves the catalogue out of context.
    assert!(html.contains("does not add the skill catalogue"), "{html}");
}

#[test]
fn tab_counts_follow_loaded_details() {
    let details = details();
    assert_eq!(details.tab_count(InfoTab::Bundles), Some(4));
    assert_eq!(details.tab_count(InfoTab::Tools), Some(1));
    assert_eq!(details.tab_count(InfoTab::Hooks), Some(4));
    assert_eq!(details.tab_count(InfoTab::Skills), Some(1));
    assert_eq!(details.tab_count(InfoTab::Overview), None);

    let failed = DefinitionDetails::unavailable("offline".into());
    assert_eq!(failed.tab_count(InfoTab::Tools), None);
    let html = render(|_| render_failure("offline"));
    assert!(html.contains("Unavailable: offline"), "{html}");
}

fn details_root() -> Element {
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        state.connection = "Connected".into();
        let session: Session = serde_json::from_value(json!({
            "id": 1, "forked_from": null, "name": "work", "working_directory": "/work",
            "probe_id": "00000000-0000-0000-0000-000000000001",
            "model_config": {"model": "openrouter/test/model", "effort": "Medium"},
            "busy": false, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        let agent: Agent = serde_json::from_value(json!({
            "id": 2, "definition_id": 1, "session_id": 1, "forked_from": null,
            "subagent_parent_id": null, "name": "worker", "interactive": true, "hidden": false,
            "state": "Idle", "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
            "next_event_id": 1, "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        state.sessions = vec![session].into();
        state.agents.insert(SessionId(1), vec![agent].into());
        state.workspace.open(
            TranscriptTarget {
                session_id: SessionId(1),
                agent_id: AgentId(2),
            },
            true,
        );
        state.sidebar.expanded_mode = SidebarMode::Details;
        state
    });
    use_context_provider(|| ui);
    rsx! { crate::views::Sidebar {} }
}

fn new_listeners(edits: &[Mutation], name: &str) -> Vec<dioxus::core::ElementId> {
    edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name: event, id } if event == name => Some(*id),
            _ => None,
        })
        .collect()
}

fn dispatch(dom: &mut VirtualDom, name: &str, id: dioxus::core::ElementId) -> Vec<Mutation> {
    use dioxus::html::{PlatformEventData, SerializedMouseData};
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default()))
            as Rc<dyn std::any::Any>,
        true,
    );
    dom.runtime().handle_event(name, event, id);
    dom.render_immediate_to_vec().edits
}

#[test]
fn agent_context_menu_opens_info() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(details_root);
    let edits = dom.rebuild_to_vec().edits;
    // The probe name has its own menu; the agent row comes after it.
    let agent_row = *new_listeners(&edits, "contextmenu").last().unwrap();

    let edits = dispatch(&mut dom, "contextmenu", agent_row);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("role=\"menu\""), "{html}");
    assert!(
        html.contains(">Info<") && !html.contains(">Rename<"),
        "{html}"
    );
    // Backdrop, menu, then the item.
    let info = new_listeners(&edits, "click")[2];

    dispatch(&mut dom, "click", info);
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("role=\"menu\""), "{html}");
    assert!(html.contains("aria-label=\"Agent information\""), "{html}");
    assert!(
        html.contains("worker") && html.contains("Overview"),
        "{html}"
    );
}

fn browser_root() -> Element {
    let ui = use_signal(|| {
        let mut state = Ui::new(DesktopSettings::default());
        state.connection = "Connected".into();
        let session: Session = serde_json::from_value(json!({
            "id": 1, "forked_from": null, "name": "work", "working_directory": "/work",
            "probe_id": "00000000-0000-0000-0000-000000000001",
            "model_config": {"model": "openrouter/test/model", "effort": "Medium"},
            "busy": false, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap();
        state.upsert_session(session);
        state
    });
    use_context_provider(|| ui);
    rsx! { crate::views::Sidebar {} }
}

#[test]
fn session_context_menu_renames_in_place() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    // The folder's probe name has its own menu; find the session row's.
    let rows = new_listeners(
        &VirtualDom::new(browser_root).rebuild_to_vec().edits,
        "contextmenu",
    );
    let (mut dom, edits) = rows
        .into_iter()
        .find_map(|row| {
            let mut dom = VirtualDom::new(browser_root);
            dom.rebuild_in_place();
            let edits = dispatch(&mut dom, "contextmenu", row);
            dioxus_ssr::render(&dom)
                .contains(">Show details<")
                .then_some((dom, edits))
        })
        .expect("a session row menu");
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains(">Rename<"), "{html}");
    dispatch(&mut dom, "click", new_listeners(&edits, "click")[2]);
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("aria-label=\"Session name\""), "{html}");
    assert!(html.contains("value=\"work\""), "{html}");
    assert!(!html.contains("class=\"session\""), "{html}");
}
