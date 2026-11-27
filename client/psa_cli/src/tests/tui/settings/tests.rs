use super::*;

#[test]
fn defaults_match_the_desktop_client() {
    let tui = TuiPreferences::default();
    let desktop = client_shared::UiPreferences::default();
    assert_eq!(tui.reasoning_display, desktop.reasoning_display);
    assert_eq!(tui.turn_dividers, desktop.turn_dividers);
    assert_eq!(tui.log_level, desktop.log_level);
    assert_eq!(tui.show_harness_messages, desktop.show_harness_messages);
    assert_eq!(
        tui.hide_text_recorded_results,
        desktop.hide_text_recorded_results
    );
    assert_eq!(tui.hide_agent_statuses, desktop.hide_agent_statuses);
    assert_eq!(tui.show_hidden_agents, desktop.show_hidden_agents);
}

#[test]
fn partial_files_fill_in_defaults() {
    let settings: TuiSettings = toml::from_str(
        r#"
        [preferences]
        theme = "Light"
        tool_output_lines = 7
        "#,
    )
    .unwrap();
    assert_eq!(settings.preferences.theme, Theme::Light);
    assert_eq!(settings.preferences.tool_output_lines, 7);
    assert_eq!(
        settings.preferences.turn_dividers,
        TurnDividerDisplay::Labeled
    );
}

#[test]
fn rows_cycle_both_ways_and_reset_unknown_values() {
    let mut preferences = TuiPreferences::default();
    SettingRow::Theme.cycle(&mut preferences, false);
    assert_eq!(preferences.theme, Theme::Monochrome);
    SettingRow::Theme.cycle(&mut preferences, true);
    assert_eq!(preferences.theme, Theme::Dark);

    SettingRow::AgentStatuses.cycle(&mut preferences, true);
    assert!(!preferences.hide_agent_statuses);
    assert_eq!(SettingRow::AgentStatuses.value(&preferences), "shown");

    preferences.tool_output_lines = 7;
    SettingRow::ToolOutputLines.cycle(&mut preferences, true);
    assert_eq!(preferences.tool_output_lines, 0);
    assert_eq!(SettingRow::ToolOutputLines.value(&preferences), "hidden");
}

#[test]
fn details_show_everything_but_keep_appearance() {
    let preferences = TuiPreferences {
        theme: Theme::Light,
        reasoning_display: ReasoningDisplay::Hidden,
        log_level: TranscriptLogLevel::Hidden,
        tool_output_lines: 0,
        max_width: 100,
        ..TuiPreferences::default()
    };
    let details = preferences.with_details();
    assert_eq!(details.theme, Theme::Light);
    assert_eq!(details.max_width, 100);
    assert_eq!(details.reasoning_display, ReasoningDisplay::Shown);
    assert_eq!(details.log_level, TranscriptLogLevel::Info);
    assert!(details.tool_output_lines >= 60);
}

#[tokio::test]
async fn saves_privately_and_loads_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = settings_path(directory.path());
    assert_eq!(load_settings(&path).await.unwrap(), TuiSettings::default());

    let mut settings = TuiSettings::default();
    settings.preferences.theme = Theme::Monochrome;
    settings.preferences.max_width = 120;
    save_settings(&path, &settings).await.unwrap();
    assert_eq!(load_settings(&path).await.unwrap(), settings);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let leftovers = std::fs::read_dir(directory.path()).unwrap().count();
    assert_eq!(leftovers, 1, "no temporary file remains");
}

#[tokio::test]
async fn malformed_files_are_reported_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = settings_path(directory.path());
    std::fs::write(&path, "preferences = 3").unwrap();
    assert!(matches!(
        load_settings(&path).await,
        Err(SettingsError::Invalid(_))
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "preferences = 3");
}
