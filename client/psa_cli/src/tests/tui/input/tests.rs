use super::*;
use crate::tui::app::AppEvent;
use crate::tui::network::Network;
use client_shared::ConnectionSettings;
use crossterm::event::KeyEventState;
use psa::api::{
    AgentId, AskQuestionRequest, ClientQuery, ClientQueryId, ClientQueryRequest, ClientQuestion,
    SessionId,
};

fn key(code: KeyCode) -> Event {
    key_with(code, KeyModifiers::NONE)
}

fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_terminal_event(key(KeyCode::Char(character)));
    }
}

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

fn dialog(choices: &[&str]) -> QuestionDialog {
    QuestionDialog::new(ClientQueryRequest {
        id: ClientQueryId::new(),
        session_id: SessionId(1),
        agent_id: AgentId(1),
        payload: ClientQuery::AskQuestion(AskQuestionRequest {
            questions: vec![
                ClientQuestion {
                    question: "Which?".into(),
                    choices: choices.iter().map(|c| c.to_string()).collect(),
                },
                ClientQuestion {
                    question: "Why?".into(),
                    choices: Vec::new(),
                },
            ],
        }),
    })
}

fn press(dialog: &mut QuestionDialog, code: KeyCode) -> Option<Vec<String>> {
    let Event::Key(event) = key(code) else {
        unreachable!()
    };
    handle_question_key(dialog, event)
}

#[test]
fn questions_collect_a_suggestion_then_typed_text() {
    let mut dialog = dialog(&["red", "blue"]);
    assert_eq!(
        press(&mut dialog, KeyCode::Enter),
        None,
        "blank answers wait"
    );
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Down);
    assert_eq!(dialog.choice, Some(1));
    assert_eq!(press(&mut dialog, KeyCode::Enter), None);
    assert_eq!(dialog.index, 1);

    press(&mut dialog, KeyCode::Char('n'));
    press(&mut dialog, KeyCode::Char('o'));
    assert_eq!(
        press(&mut dialog, KeyCode::Enter),
        Some(vec!["blue".to_owned(), "no".to_owned()])
    );
}

#[test]
fn typing_replaces_a_highlighted_suggestion() {
    let mut dialog = dialog(&["red"]);
    press(&mut dialog, KeyCode::Up);
    assert_eq!(dialog.choice, Some(0));
    press(&mut dialog, KeyCode::Char('x'));
    assert_eq!(dialog.choice, None);
    press(&mut dialog, KeyCode::Enter);
    assert_eq!(dialog.answers, vec!["x".to_owned()]);
}

#[tokio::test]
async fn quit_runs_from_the_command_line_and_twice_ctrl_c() {
    let mut app = new_app();
    type_text(&mut app, "/quit");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.quit);

    let mut app = new_app();
    type_text(&mut app, "draft");
    app.handle_terminal_event(key_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.composer.is_empty() && !app.quit);
    app.handle_terminal_event(key_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.quit);
    app.handle_terminal_event(key_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.quit);
}

fn menu_labels(app: &App) -> Vec<String> {
    app.command_menu()
        .map(|menu| menu.candidates.into_iter().map(|c| c.label).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn completion_offers_only_commands_usable_without_a_session() {
    let mut app = new_app();
    type_text(&mut app, "/");
    let names = menu_labels(&app);
    assert!(names.contains(&"/sessions".into()) && names.contains(&"/new".into()));
    assert!(!names.contains(&"/compact".into()), "needs an open agent");
    assert!(!names.contains(&"/sandbox".into()), "needs a session");
    assert!(!names.contains(&"/agent".into()), "GUI only");
    assert!(!names.contains(&"/server".into()), "no local server");

    type_text(&mut app, "sess");
    app.handle_terminal_event(key(KeyCode::Tab));
    assert_eq!(app.composer.text(), "/sessions");
}

#[tokio::test]
async fn enter_runs_the_highlighted_command_and_opens_its_picker() {
    let mut app = new_app();
    type_text(&mut app, "/sess");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.composer.is_empty());
    assert!(matches!(app.overlay, Some(Overlay::Picker(ref p)) if p.loading));
    app.handle_terminal_event(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
}

#[tokio::test]
async fn goal_subcommands_complete_and_free_text_is_sent() {
    let mut app = new_app();
    app.open_agent(SessionId(1), AgentId(1));
    type_text(&mut app, "/go");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert_eq!(app.composer.text(), "/goal ", "Enter opens the subcommands");
    assert_eq!(menu_labels(&app), ["show", "pause", "resume", "clear"]);

    app.handle_terminal_event(key(KeyCode::Down));
    app.handle_terminal_event(key(KeyCode::Tab));
    assert_eq!(app.composer.text(), "/goal pause ");
    assert!(app.command_menu().is_none(), "the reason is free text");

    app.clear_composer();
    type_text(&mut app, "/goal fix the tests");
    assert!(app.command_menu().is_none());
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.composer.is_empty(), "sent as a goal");
}

#[tokio::test]
async fn new_starts_a_draft_and_help_opens_the_commands_tab() {
    let mut app = new_app();
    type_text(&mut app, "/new");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(
        app.draft.is_some() && app.overlay.is_none(),
        "no name prompt"
    );

    type_text(&mut app, "/help");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Settings {
            tab: SettingsTab::Commands,
            ..
        })
    ));
    app.handle_terminal_event(key(KeyCode::Tab));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Settings {
            tab: SettingsTab::Appearance,
            ..
        })
    ));
}

#[tokio::test]
async fn messages_need_an_open_session_and_slashes_can_be_literal() {
    let mut app = new_app();
    type_text(&mut app, "hello");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert_eq!(app.composer.text(), "hello", "kept for after /sessions");
    assert!(matches!(
        app.message,
        Some(crate::tui::app::StatusMessage::Error(_))
    ));

    app.composer.clear();
    type_text(&mut app, "//help");
    assert!(app.command_menu().is_none());
}

#[tokio::test]
async fn rename_prompt_needs_a_name_takes_one_line_and_esc_cancels() {
    let mut app = new_app();
    app.run_command(client_shared::CommandId::Rename, String::new());
    assert!(app.overlay.is_none(), "refused without an open session");

    app.overlay = Some(Overlay::Prompt(crate::tui::app::PromptDialog {
        action: crate::tui::app::PromptAction::RenameSession,
        title: "Rename session",
        input: Composer::default(),
    }));
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.overlay.is_some(), "an empty name is not submitted");
    type_text(&mut app, "new name");
    app.handle_terminal_event(Event::Paste("\nmore".into()));
    let Some(Overlay::Prompt(dialog)) = &app.overlay else {
        panic!("prompt closed");
    };
    assert_eq!(dialog.input.text(), "new name more");
    app.handle_terminal_event(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
}

#[tokio::test]
async fn settings_change_live_and_close_with_esc() {
    let mut app = new_app();
    type_text(&mut app, "/settings");
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(matches!(
        app.overlay,
        Some(Overlay::Settings {
            tab: SettingsTab::Appearance,
            selected: 0,
            changed: false
        })
    ));

    app.handle_terminal_event(key(KeyCode::Right));
    assert_eq!(
        app.settings.preferences.theme,
        crate::tui::theme::Theme::Light
    );
    app.handle_terminal_event(key(KeyCode::Down));
    app.handle_terminal_event(key(KeyCode::Left));
    assert_eq!(
        app.settings.preferences.reasoning_display,
        client_shared::ReasoningDisplay::Hidden
    );
    assert!(matches!(
        app.overlay,
        Some(Overlay::Settings {
            selected: 1,
            changed: true,
            ..
        })
    ));
    app.handle_terminal_event(key(KeyCode::Esc));
    assert!(app.overlay.is_none());
}

fn model(slug: &str, name: &str, kind: &str, pinned: bool) -> psa::api::ModelInfo {
    serde_json::from_value(serde_json::json!({
        "model": slug, "name": name, "kind": kind,
        "input_modalities": [], "context_window": null, "max_output_tokens": null,
        "cost": {"tiers": []}, "reasoning": false, "reasoning_options": [],
        "authed": true, "pinned": pinned, "refreshed_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

#[tokio::test]
async fn pin_pickers_list_one_kind_and_stay_open_on_enter() {
    let mut app = new_app();
    app.models = vec![
        model("openrouter/alpha", "Alpha", "Chat", true),
        model("openrouter/judge", "Judge", "Decision", false),
        model("openrouter/beta", "Beta", "Chat", false),
    ];
    type_text(&mut app, "/pinned-models");
    app.handle_terminal_event(key(KeyCode::Enter));
    let Some(Overlay::Picker(picker)) = &app.overlay else {
        panic!("no pin picker");
    };
    let labels: Vec<_> = picker.visible().iter().map(|i| i.label.clone()).collect();
    assert_eq!(labels, ["★ Alpha", "☆ Beta"]);

    app.handle_terminal_event(key(KeyCode::Down));
    app.handle_terminal_event(key(KeyCode::Enter));
    assert!(app.overlay.is_some(), "pinning keeps the list open");

    // The server's answer stars the model where it is.
    app.apply(AppEvent::ModelPinned {
        model: llm_api::providers::Model::parse_slug("openrouter/beta").unwrap(),
        pinned: true,
    });
    let Some(Overlay::Picker(picker)) = &app.overlay else {
        panic!("picker closed");
    };
    assert_eq!(picker.visible()[1].label, "★ Beta");
    assert_eq!(picker.selected_index(), 1);

    app.overlay = None;
    type_text(&mut app, "/pinned-decision-models");
    app.handle_terminal_event(key(KeyCode::Enter));
    let Some(Overlay::Picker(picker)) = &app.overlay else {
        panic!("no decision pin picker");
    };
    assert_eq!(picker.visible()[0].label, "☆ Judge");
}

#[tokio::test]
async fn model_list_collapses_after_leaving_model() {
    let mut app = new_app();
    app.open_agent(SessionId(1), AgentId(1));
    type_text(&mut app, "/model ");
    app.show_all_models = true;
    app.handle_terminal_event(key(KeyCode::Backspace));
    assert!(!app.show_all_models);
}
