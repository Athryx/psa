use super::*;
use crate::tui::{network::Network, render::draw};
use client_shared::ConnectionSettings;
use ratatui::{Terminal, backend::TestBackend};

fn labels(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("agent{index}")).collect()
}

#[test]
fn every_agent_shows_when_they_fit() {
    assert_eq!(fit_agent_window(&labels(3), 1, 0, 80), (0, 3));
}

#[test]
fn the_window_moves_only_when_the_open_agent_would_leave_it() {
    // Each agent takes 8 columns, plus 2 between them and 2 for each arrow.
    let labels = labels(10);
    let (start, end) = fit_agent_window(&labels, 0, 0, 28);
    assert_eq!((start, end), (0, 2));
    assert_eq!(
        fit_agent_window(&labels, 1, start, 28),
        (0, 2),
        "still visible"
    );

    let (start, end) = fit_agent_window(&labels, 2, 0, 28);
    assert!(start > 0 && (start..end).contains(&2), "{start}..{end}");

    assert_eq!(fit_agent_window(&labels, 9, 0, 28).1, 10, "the last one");
    assert_eq!(
        fit_agent_window(&labels, 0, 7, 28).0,
        0,
        "back to the first"
    );
}

#[test]
fn a_name_wider_than_the_bar_still_shows() {
    let labels = vec!["a-very-long-agent-name".to_owned(), "b".to_owned()];
    assert_eq!(fit_agent_window(&labels, 0, 0, 5), (0, 1));
    assert_eq!(fit_agent_window(&labels, 1, 0, 5), (1, 2));
}

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

fn bottom_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (height - 2..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

#[tokio::test]
async fn a_new_session_shows_its_directory_model_and_name() {
    let mut app = new_app();
    app.start_draft();
    app.draft.as_mut().unwrap().directory = "/work/project".into();
    app.draft.as_mut().unwrap().name = Some("Demo".into());

    let lines = bottom_lines(&mut app, 60, 12);
    assert!(lines[0].starts_with(" /work/project"), "{lines:?}");
    assert!(
        lines[0].trim_end().ends_with("Unknown model (? / ?)"),
        "{lines:?}"
    );
    assert!(lines[1].starts_with(" Demo › no agents yet"), "{lines:?}");
}

#[tokio::test]
async fn without_a_session_the_bar_says_so() {
    let mut app = new_app();
    let lines = bottom_lines(&mut app, 40, 12);
    assert!(lines[0].starts_with(" No session"), "{lines:?}");
}
