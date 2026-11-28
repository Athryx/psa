//! Full-screen terminal client: one agent's transcript, an input line with
//! slash commands, and dialogs for pickers, details, and agent questions.
//!
//! The app task owns all state. Terminal input and background server tasks
//! reach it as events; it redraws at most once per frame after a change.
mod app;
mod commands;
mod composer;
mod draft;
mod input;
mod network;
mod picker;
mod render;
mod settings;
mod status_bar;
mod text;
mod theme;
mod transcript_view;

use crate::CliError;
use app::App;
use client_shared::{ConnectionSettings, LocalServerManager, LocalServerStatus};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::StreamExt;
use network::Network;
use std::time::Duration;

const FRAME: Duration = Duration::from_millis(33);

/// Where the TUI connects: an explicit server, or the data directory's local
/// server, started when it is not running.
pub enum Target {
    Remote(ConnectionSettings),
    Local(LocalServerManager),
}

/// Run the TUI; its settings live in `data_dir`.
pub async fn run(target: Target, data_dir: &std::path::Path) -> Result<(), CliError> {
    let settings_path = settings::settings_path(data_dir);
    let settings = settings::load_settings(&settings_path)
        .await
        .map_err(|error| CliError::Settings(error.to_string()))?;
    let (connection, local_server) = connect(target).await?;
    let client = psa::api::Client::new(&connection.api_url, &connection.client_key)
        .map_err(|error| CliError::Connect(error.to_string()))?;
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        Network::new(client, events),
        &connection,
        local_server,
        settings,
        settings_path,
    );
    app.start();

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    // Without it, Alt+[ starts an escape sequence instead of being a key.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        let _ = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(flags));
    }
    let result = run_event_loop(&mut terminal, &mut app, &mut receiver).await;
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();

    if let Some(message) = app.exit_message.take() {
        println!("{message}");
    }
    result.map_err(CliError::from)
}

async fn connect(
    target: Target,
) -> Result<(ConnectionSettings, Option<LocalServerManager>), CliError> {
    let manager = match target {
        Target::Remote(connection) => return Ok((connection, None)),
        Target::Local(manager) => manager,
    };
    let status = match manager.status().await? {
        LocalServerStatus::Stopped => {
            eprintln!("Starting the local server…");
            manager.start().await?
        }
        running => running,
    };
    match status {
        LocalServerStatus::Running { connection } => Ok((connection, Some(manager))),
        LocalServerStatus::Stopped => {
            Err(CliError::Connect("the local server did not start".into()))
        }
    }
}

async fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<app::AppEvent>,
) -> std::io::Result<()> {
    let mut input = EventStream::new();
    let mut frames = tokio::time::interval(FRAME);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    while !app.quit {
        tokio::select! {
            event = input.next() => match event {
                Some(event) => {
                    app.handle_terminal_event(event?);
                    dirty = true;
                }
                None => return Ok(()),
            },
            Some(event) = receiver.recv() => {
                app.apply(event);
                dirty = true;
            }
            _ = frames.tick() => {
                app.tick += 1;
                // Animate the spinner a few times a second.
                if app.is_animating() && app.tick.is_multiple_of(3) {
                    dirty = true;
                }
                if dirty {
                    terminal.draw(|frame| render::draw(frame, app))?;
                    dirty = false;
                }
            }
        }
    }
    Ok(())
}
