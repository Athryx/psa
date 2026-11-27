use super::*;
use crate::{COMMANDS, CommandContext};

fn model(slug: &str, name: &str, pinned: bool, efforts: &[&str]) -> ModelInfo {
    let options: Vec<_> = efforts
        .iter()
        .map(|effort| serde_json::json!({"effort": effort, "wire_effort": effort}))
        .collect();
    serde_json::from_value(serde_json::json!({
        "model": slug, "name": name,
        "input_modalities": [], "context_window": null, "max_output_tokens": null,
        "cost": {"tiers": []}, "reasoning": true,
        "reasoning_options": options,
        "authed": true, "pinned": pinned, "refreshed_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

fn catalog() -> Vec<ModelInfo> {
    vec![
        model("openrouter/alpha", "Alpha", true, &["Low", "High"]),
        model("openrouter/beta", "Beta", false, &["Medium"]),
        model("openrouter/gamma", "Gamma", false, &["High"]),
        model("openrouter/delta", "Delta", true, &["Low"]),
    ]
}

fn config(slug: &str, effort: &str) -> ModelConfig {
    serde_json::from_value(serde_json::json!({"model": slug, "effort": effort})).unwrap()
}

fn context<'a>(models: &'a [ModelInfo], current: Option<&'a ModelConfig>) -> MenuContext<'a> {
    MenuContext {
        frontend: Frontend::Tui,
        context: CommandContext::Agent,
        models,
        session_model: current,
        show_all_models: false,
        local_server: true,
        root: None,
        probes: &[],
    }
}

fn labels(menu: &CommandMenu) -> Vec<&str> {
    menu.candidates.iter().map(|c| c.label.as_str()).collect()
}

#[test]
fn names_open_argument_menus_or_run() {
    let models = catalog();
    let cx = context(&models, None);
    let menu = build_command_menu("/mo", &cx).unwrap();
    assert_eq!(menu.command, None);
    assert_eq!(
        menu.candidates[0].action,
        MenuAction::Fill("/model ".into())
    );
    assert_eq!(menu.candidates[0].completion.as_deref(), Some("/model "));

    let menu = build_command_menu("/info", &cx).unwrap();
    assert_eq!(
        menu.candidates[0].action,
        MenuAction::Run {
            id: CommandId::Info,
            arguments: String::new()
        }
    );
    assert_eq!(menu.candidates[0].completion.as_deref(), Some("/info"));
    assert!(build_command_menu("//mo", &cx).is_none());
    assert!(build_command_menu("hello", &cx).is_none());
    assert!(build_command_menu("/nothing", &cx).is_none());
}

#[test]
fn names_respect_scope_and_server_availability() {
    let models = catalog();
    let mut cx = context(&models, None);
    cx.context = CommandContext::NoSession;
    cx.local_server = false;
    let names: Vec<_> = build_command_menu("/", &cx)
        .unwrap()
        .candidates
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert!(names.contains(&"/sessions".to_owned()));
    assert!(!names.contains(&"/compact".to_owned()));
    assert!(!names.contains(&"/server".to_owned()));
    assert!(build_command_menu("/server ", &cx).is_none());
}

#[test]
fn goal_offers_show_and_its_subcommands_then_gets_out_of_the_way() {
    let models = catalog();
    let cx = context(&models, None);
    let menu = build_command_menu("/goal ", &cx).unwrap();
    assert_eq!(labels(&menu), ["show", "pause", "resume", "clear"]);
    assert_eq!(
        menu.candidates[0].action,
        MenuAction::Run {
            id: CommandId::Goal,
            arguments: String::new()
        }
    );
    assert_eq!(
        menu.candidates[1].completion.as_deref(),
        Some("/goal pause ")
    );
    assert_eq!(
        menu.candidates[2].completion.as_deref(),
        Some("/goal resume")
    );

    let menu = build_command_menu("/goal p", &cx).unwrap();
    assert_eq!(labels(&menu), ["pause"]);
    assert!(build_command_menu("/goal pause waiting", &cx).is_none());
    assert!(build_command_menu("/goal fix the tests", &cx).is_none());
}

#[test]
fn sandbox_and_server_list_their_actions() {
    let models = catalog();
    let cx = context(&models, None);
    let menu = build_command_menu("/sandbox ", &cx).unwrap();
    assert_eq!(labels(&menu), ["start", "stop"]);
    assert_eq!(
        menu.candidates[1].action,
        MenuAction::Run {
            id: CommandId::Sandbox,
            arguments: "stop".into()
        }
    );
    let menu = build_command_menu("/server re", &cx).unwrap();
    assert_eq!(labels(&menu), ["restart"]);
}

#[test]
fn text_commands_have_no_argument_menu() {
    let models = catalog();
    let cx = context(&models, None);
    assert!(build_command_menu("/rename ", &cx).is_none());
    assert!(build_command_menu("/compact focus", &cx).is_none());
}

#[test]
fn models_list_pinned_and_current_with_a_toggle_last() {
    let models = catalog();
    let current = config("openrouter/gamma", "High");
    let cx = context(&models, Some(&current));
    let menu = build_command_menu("/model ", &cx).unwrap();
    assert_eq!(
        labels(&menu),
        ["Alpha", "Delta", "Gamma", "Show all models (1 more)"]
    );
    assert_eq!(menu.candidates[2].detail, "openrouter/gamma · current");
    assert_eq!(menu.candidates[3].kind, CandidateKind::Toggle);
    assert_eq!(menu.candidates[3].action, MenuAction::ToggleAllModels);
    assert_eq!(
        menu.note.as_deref(),
        Some("Current: openrouter/gamma · high")
    );

    let all = MenuContext {
        show_all_models: true,
        ..cx
    };
    let menu = build_command_menu("/model ", &all).unwrap();
    assert_eq!(
        labels(&menu),
        ["Alpha", "Delta", "Gamma", "Beta", "Show pinned only"]
    );
}

#[test]
fn typed_words_narrow_the_list_and_count_hidden_matches() {
    let models = catalog();
    let current = config("openrouter/alpha", "High");
    let cx = context(&models, Some(&current));
    let menu = build_command_menu("/model ta", &cx).unwrap();
    // Delta is pinned; Beta matches but is hidden.
    assert_eq!(labels(&menu), ["Delta", "Show all models (1 more match)"]);

    let menu = build_command_menu("/model openrouter beta", &cx).unwrap();
    assert_eq!(labels(&menu), ["Show all models (1 more match)"]);
    let menu = build_command_menu("/model zzz", &cx).unwrap();
    assert_eq!(labels(&menu), ["Show all models"]);
    assert_eq!(menu.empty_hint, "No matching models.");
}

#[test]
fn choosing_a_model_keeps_a_supported_effort() {
    let models = catalog();
    let current = config("openrouter/alpha", "High");
    let cx = MenuContext {
        show_all_models: true,
        ..context(&models, Some(&current))
    };
    let menu = build_command_menu("/model ", &cx).unwrap();
    let effort_of = |label: &str| match &menu
        .candidates
        .iter()
        .find(|c| c.label == label)
        .unwrap()
        .action
    {
        MenuAction::SetModel(config) => config.effort,
        other => panic!("{other:?}"),
    };
    assert_eq!(effort_of("Gamma"), Effort::High);
    assert_eq!(effort_of("Delta"), Effort::Low);
}

#[test]
fn no_toggle_when_every_model_is_listed() {
    let models = vec![model("openrouter/alpha", "Alpha", true, &["Low"])];
    let cx = context(&models, None);
    let menu = build_command_menu("/model ", &cx).unwrap();
    assert_eq!(labels(&menu), ["Alpha"]);
    let none: Vec<ModelInfo> = Vec::new();
    let menu = build_command_menu("/model ", &context(&none, None)).unwrap();
    assert!(menu.candidates.is_empty());
    assert_eq!(menu.empty_hint, "No pinned models. Pin models in settings.");
}

#[test]
fn efforts_come_from_the_current_model() {
    let models = catalog();
    let current = config("openrouter/alpha", "Low");
    let cx = context(&models, Some(&current));
    let menu = build_command_menu("/effort ", &cx).unwrap();
    assert_eq!(labels(&menu), ["low", "high"]);
    assert_eq!(
        menu.candidates[1].action,
        MenuAction::SetEffort(Effort::High)
    );
    let menu = build_command_menu("/effort med", &cx).unwrap();
    assert!(menu.candidates.is_empty());

    let loading = context(&models, None);
    let menu = build_command_menu("/effort ", &loading).unwrap();
    assert!(menu.candidates.is_empty());
    assert_eq!(menu.empty_hint, "Choose a model with /model first.");
}

#[test]
fn effort_configs_require_a_supported_effort() {
    let models = catalog();
    let current = config("openrouter/alpha", "Low");
    assert!(build_effort_config(&[], Some(&current), Effort::Low).is_none());
    assert!(build_effort_config(&models, None, Effort::High).is_none());
    assert!(build_effort_config(&models, Some(&current), Effort::Medium).is_none());
    let updated = build_effort_config(&models, Some(&current), Effort::High).unwrap();
    assert_eq!(updated.model, current.model);
    assert_eq!(updated.effort, Effort::High);
}

#[test]
fn every_argument_menu_command_has_a_menu() {
    let models = catalog();
    let current = config("openrouter/alpha", "Low");
    let cx = MenuContext {
        frontend: Frontend::Gui,
        ..context(&models, Some(&current))
    };
    for spec in COMMANDS.iter().filter(|spec| spec.has_argument_menu()) {
        if !cx.offers(spec) {
            continue;
        }
        assert!(
            build_command_menu(&format!("/{} ", spec.name), &cx).is_some(),
            "/{}",
            spec.name
        );
    }
}

fn agent_config(
    name: &str,
    group: serde_json::Value,
    models: serde_json::Value,
) -> psa::api::AgentConfigStatus {
    serde_json::from_value(serde_json::json!({
        "config": {"config_id": {"config_group": group, "name": name}, "selectable": true,
          "models": models, "default": false},
        "status": {"missing_components": []}
    }))
    .unwrap()
}

fn probe(id: u8, hostname: &str, host: bool, active: bool) -> Probe {
    serde_json::from_value(serde_json::json!({
        "id": format!("00000000-0000-0000-0000-0000000000{id:02}"),
        "hostname": hostname, "os": "linux", "architecture": "x86_64",
        "label": null, "data_dir": "/data", "home_dir": "/home/me",
        "host": host, "sandboxes": false, "is_active": active,
        "created_at": "2026-01-01T00:00:00Z", "last_connected_at": "2026-01-01T00:00:00Z",
    }))
    .unwrap()
}

#[test]
fn agent_lists_configurations_and_then_their_model_policies() {
    let models = catalog();
    let mut root = crate::DraftRoot {
        ready: true,
        ..Default::default()
    };
    root.apply_configs(
        vec![
            agent_config("worker", serde_json::json!("Global"), serde_json::json!(["Global"])),
            agent_config(
                "local",
                serde_json::json!({"AgentsDir": {"path": "/work/.agents", "probe_id": "00000000-0000-0000-0000-000000000001"}}),
                serde_json::json!(["Global", {"Fixed": {"model": "openrouter/alpha", "effort": {"Fixed": "Low"}}}]),
            ),
        ],
        "worker",
    );
    let cx = MenuContext {
        context: CommandContext::SessionDraft,
        root: Some(&root),
        ..context(&models, None)
    };
    let menu = build_command_menu("/agent ", &cx).unwrap();
    assert_eq!(labels(&menu), ["worker", "local"]);
    assert_eq!(menu.candidates[0].detail, "global · current");
    assert_eq!(menu.candidates[1].detail, "/work/.agents");
    assert_eq!(
        menu.candidates[1].completion.as_deref(),
        Some("/agent local")
    );
    let MenuAction::ChooseConfig(id) = &menu.candidates[1].action else {
        panic!("not a configuration choice");
    };

    root.select_config(id.clone());
    let menu = build_root_model_menu(&root);
    assert_eq!(menu.candidates.len(), 2);
    assert!(matches!(
        menu.candidates[1].action,
        MenuAction::ChooseRootModel(ModelSelection::Fixed { .. })
    ));
}

#[test]
fn probe_lists_connected_probes_by_name() {
    let models = catalog();
    let probes = [
        probe(1, "laptop", true, true),
        probe(2, "builder", false, true),
        probe(3, "offline", false, false),
    ];
    let cx = MenuContext {
        context: CommandContext::SessionDraft,
        probes: &probes,
        ..context(&models, None)
    };
    let menu = build_command_menu("/probe ", &cx).unwrap();
    assert_eq!(labels(&menu), ["laptop", "builder"]);
    assert_eq!(menu.candidates[0].detail, "linux/x86_64 · this machine");
    assert_eq!(
        labels(&build_command_menu("/probe bui", &cx).unwrap()),
        ["builder"]
    );

    let agent = MenuContext {
        context: CommandContext::Agent,
        ..cx
    };
    assert!(
        build_command_menu("/probe ", &agent).is_none(),
        "drafts only"
    );
}
