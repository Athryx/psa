use crate::tui::{app::AppEvent, network::Network};
use client_shared::{CandidateKind, CommandContext, ConnectionSettings, MenuAction};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use psa::api::{AgentConfigStatus, ConfigGroup, Probe};
use serde_json::json;
use std::path::PathBuf;

use super::super::app::App;

fn new_app() -> App {
    // Requests fail to connect; the tests only look at local state.
    let connection = ConnectionSettings {
        api_url: "http://127.0.0.1:9".into(),
        client_key: "key".into(),
    };
    let client = psa::api::Client::new(&connection.api_url, &connection.client_key).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    App::new(
        Network::new(client, events),
        &connection,
        None,
        crate::tui::settings::TuiSettings::default(),
        std::env::temp_dir().join("psa-tui-test-settings.toml"),
    )
}

fn probe(id: u8, hostname: &str, host: bool) -> Probe {
    serde_json::from_value(json!({
        "id": format!("00000000-0000-0000-0000-0000000000{id:02}"),
        "hostname": hostname, "os": "linux", "architecture": "x86_64",
        "label": null, "data_dir": "/data", "home_dir": format!("/home/{hostname}"),
        "host": host, "sandboxes": false, "is_active": true,
        "created_at": "2026-01-01T00:00:00Z", "last_connected_at": "2026-01-01T00:00:00Z",
    }))
    .unwrap()
}

fn config(name: &str, group: serde_json::Value, models: serde_json::Value) -> AgentConfigStatus {
    serde_json::from_value(json!({
        "config": {"config_id": {"config_group": group, "name": name}, "selectable": true,
          "models": models, "default": false},
        "status": {"missing_components": []}
    }))
    .unwrap()
}

fn agents_dir(path: &str) -> serde_json::Value {
    json!({"AgentsDir": {"path": path, "probe_id": "00000000-0000-0000-0000-000000000001"}})
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_terminal_event(key(KeyCode::Char(character)));
    }
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// A draft on the host probe with its configurations loaded.
fn ready_app() -> App {
    let mut app = new_app();
    app.start();
    app.apply(AppEvent::Probes(vec![
        probe(1, "laptop", true),
        probe(2, "builder", false),
    ]));
    let generation = app.draft.as_ref().unwrap().generation;
    let local = ConfigGroup::AgentsDir {
        path: "/work/.agents".into(),
        probe_id: probe(1, "laptop", true).metadata.id,
    };
    app.apply(AppEvent::DraftConfigs {
        generation,
        agents_dir: Some(local),
        configs: vec![
            config("worker", json!("Global"), json!(["Global"])),
            config("local", agents_dir("/work/.agents"), json!(["Global", "Parent"])),
            config(
                "elsewhere",
                agents_dir("/else/.agents"),
                json!(["Global"]),
            ),
            config(
                "picky",
                json!("Global"),
                json!(["Global", {"Fixed": {"model": "openrouter/alpha", "effort": {"Fixed": "Low"}}}]),
            ),
        ],
        default_name: "worker".into(),
    });
    app
}

#[tokio::test]
async fn startup_is_a_draft_on_the_host_probe_in_the_current_directory() {
    let mut app = new_app();
    app.start();
    assert_eq!(app.command_context(), CommandContext::SessionDraft);
    assert!(app.overlay.is_none(), "no sessions picker on startup");
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.directory, std::env::current_dir().unwrap());
    assert_eq!(draft.probe_id, None, "probes are not loaded yet");

    app.apply(AppEvent::Probes(vec![
        probe(2, "builder", false),
        probe(1, "laptop", true),
    ]));
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.probe_id, Some(probe(1, "laptop", true).metadata.id));
    assert!(draft.root.loading);
}

#[tokio::test]
async fn configurations_are_global_and_this_directorys_own() {
    let app = ready_app();
    let root = &app.draft.as_ref().unwrap().root;
    let names: Vec<_> = root
        .configs
        .iter()
        .map(|c| c.config.config_id.name.as_str())
        .collect();
    assert_eq!(names, ["worker", "local", "picky"]);
    assert_eq!(
        root.selected.as_ref().unwrap().name,
        "worker",
        "server default"
    );
}

#[tokio::test]
async fn agent_with_several_policies_asks_for_one() {
    let mut app = ready_app();
    type_text(&mut app, "/select agent pick");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.choosing_root_model);
    let menu = app.command_menu().expect("model policy menu");
    assert_eq!(menu.title, "Choose the root agent model policy");
    assert_eq!(menu.candidates.len(), 2);
    assert!(
        menu.candidates
            .iter()
            .all(|c| c.kind == CandidateKind::Choice)
    );

    app.handle_terminal_event(key(KeyCode::Down));
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(!app.choosing_root_model);
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.root.selected.as_ref().unwrap().name, "picky");
    assert!(matches!(
        draft.root.model,
        Some(psa::api::ModelSelection::Fixed { .. })
    ));
}

#[tokio::test]
async fn rename_names_the_draft_without_a_request() {
    let mut app = ready_app();
    type_text(&mut app, "/rename Upload fixes");
    app.handle_terminal_event(key(KeyCode::Enter));
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.name.as_deref(), Some("Upload fixes"));

    type_text(&mut app, "/rename");
    app.handle_terminal_event(key(KeyCode::Enter));
    let Some(crate::tui::app::Overlay::Prompt(dialog)) = &app.overlay else {
        panic!("no name prompt");
    };
    assert_eq!(dialog.input.text(), "Upload fixes");
}

#[tokio::test]
async fn probe_keeps_the_directory_only_on_the_same_probe() {
    let mut app = ready_app();
    let laptop = probe(1, "laptop", true).metadata.id;
    let builder = probe(2, "builder", false).metadata.id;
    let directory = app.draft.as_ref().unwrap().directory.clone();

    app.apply_menu_action(MenuAction::ChooseProbe(laptop));
    assert_eq!(app.draft.as_ref().unwrap().directory, directory);

    app.apply_menu_action(MenuAction::ChooseProbe(builder));
    let draft = app.draft.as_ref().unwrap();
    assert_eq!(draft.probe_id, Some(builder));
    assert_eq!(draft.directory, PathBuf::from("/home/builder"));
    assert!(draft.root.loading, "the new location's configurations load");
}

#[tokio::test]
async fn session_commands_wait_for_the_first_message() {
    let mut app = ready_app();
    for command in ["/compact", "/fork", "/sandbox start", "/agents"] {
        app.clear_composer();
        type_text(&mut app, command);
        app.handle_terminal_event(key(KeyCode::Esc));
        app.handle_terminal_event(key(KeyCode::Enter));
        assert!(app.draft.is_some(), "{command}");
        assert!(
            matches!(&app.message, Some(crate::tui::app::StatusMessage::Error(text)) if text.contains("first message")),
            "{command}"
        );
    }
}

#[tokio::test]
async fn first_message_needs_a_policy_then_sends_and_restores_on_failure() {
    let mut app = ready_app();
    app.apply_menu_action(MenuAction::ChooseConfig(
        app.draft.as_ref().unwrap().root.configs[2]
            .config
            .config_id
            .clone(),
    ));
    app.choosing_root_model = false;
    type_text(&mut app, "fix the upload test");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(
        app.choosing_root_model,
        "asks for the policy instead of sending"
    );
    assert_eq!(app.composer.text(), "fix the upload test");

    app.choose_draft_root_model(psa::api::ModelSelection::Global);
    app.handle_terminal_event(key(KeyCode::Enter));
    let draft = app.draft.as_ref().unwrap();
    assert!(draft.pending && app.composer.is_empty());
    let generation = draft.generation;

    app.apply(AppEvent::DraftFailed {
        generation,
        message: "Unable to create the session".into(),
        text: "fix the upload test".into(),
    });
    assert!(!app.draft.as_ref().unwrap().pending);
    assert_eq!(app.composer.text(), "fix the upload test");
}

#[tokio::test]
async fn new_from_a_draft_starts_over_in_the_same_place() {
    let mut app = ready_app();
    app.apply_menu_action(MenuAction::ChooseProbe(
        probe(2, "builder", false).metadata.id,
    ));
    let old = app.draft.as_ref().unwrap().generation;
    app.run_command(client_shared::CommandId::NewSession, String::new());
    let draft = app.draft.as_ref().unwrap();
    assert!(draft.generation > old);
    assert_eq!(draft.directory, PathBuf::from("/home/builder"));
    assert!(draft.name.is_none());

    // Results for the old draft are ignored.
    app.apply(AppEvent::DraftConfigs {
        generation: old,
        agents_dir: None,
        configs: vec![config("stale", json!("Global"), json!(["Global"]))],
        default_name: "stale".into(),
    });
    assert!(app.draft.as_ref().unwrap().root.configs.is_empty());
}
