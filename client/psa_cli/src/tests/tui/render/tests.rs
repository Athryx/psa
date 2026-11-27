use super::*;
use crate::tui::{
    app::{AppEvent, QuestionDialog},
    network::Network,
    picker::{PickerItem, PickerValue},
};
use client_shared::ConnectionSettings;
use psa::api::{
    AgentId, AskQuestionRequest, ClientQuery, ClientQueryId, ClientQueryRequest, ClientQuestion,
    SessionId,
};
use ratatui::{Terminal, backend::TestBackend};

fn new_app() -> App {
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

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..height {
        for x in 0..width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn question() -> QuestionDialog {
    QuestionDialog::new(ClientQueryRequest {
        id: ClientQueryId::new(),
        session_id: SessionId(1),
        agent_id: AgentId(1),
        payload: ClientQuery::AskQuestion(AskQuestionRequest {
            questions: vec![ClientQuestion {
                question: "Deploy to production?".into(),
                choices: vec!["yes".into(), "no".into()],
            }],
        }),
    })
}

#[tokio::test]
async fn draws_every_overlay_at_normal_and_tiny_sizes() {
    let overlays = [
        Overlay::Question(question()),
        Overlay::Picker(Picker::new(
            "Sessions",
            vec![PickerItem {
                label: "fix tests".into(),
                detail: "2h ago".into(),
                value: PickerValue::Session(SessionId(1)),
            }],
        )),
        Overlay::Settings {
            tab: crate::tui::app::SettingsTab::Commands,
            selected: 0,
            changed: false,
        },
        Overlay::Panel {
            title: "Help".into(),
            lines: vec!["a very long line ".repeat(20)],
            scroll: 99,
        },
    ];
    for overlay in overlays {
        let mut app = new_app();
        app.overlay = Some(overlay);
        let normal = screen(&mut app, 80, 24);
        assert!(
            normal.contains("Deploy to production?")
                || normal.contains("fix tests")
                || normal.contains("/model [search]")
                || normal.contains("a very long line"),
            "{normal}"
        );
        for (width, height) in [(1, 1), (10, 4), (20, 8)] {
            screen(&mut app, width, height);
        }
    }
}

#[tokio::test]
async fn shows_the_command_menu_and_errors() {
    let mut app = new_app();
    app.composer.set_text("/se");
    let text = screen(&mut app, 80, 24);
    assert!(
        text.contains("/sessions") && !text.contains("/refresh"),
        "{text}"
    );

    app.composer.clear();
    app.apply(AppEvent::Error("Message not sent".into()));
    assert!(screen(&mut app, 80, 24).contains("Message not sent"));
    screen(&mut app, 3, 2);
}

#[tokio::test]
async fn a_failed_send_restores_the_text_only_into_an_empty_input() {
    let mut app = new_app();
    app.apply(AppEvent::SendFailed("hello".into()));
    assert_eq!(app.composer.text(), "hello");
    app.composer.set_text("newer");
    app.apply(AppEvent::SendFailed("hello".into()));
    assert_eq!(app.composer.text(), "newer");
}
