use super::*;
use serde_json::json;

#[test]
fn counts_are_abbreviated() {
    assert_eq!(describe_count(None), "?");
    assert_eq!(describe_count(Some(999)), "999");
    assert_eq!(describe_count(Some(12_345)), "12.3k");
    assert_eq!(describe_count(Some(2_500_000)), "2.5M");
}

#[test]
fn context_shows_the_share_of_the_window() {
    let status = client_shared::AgentStatus {
        model_config: None,
        context_tokens: Some(50_000),
        context_window: Some(200_000),
    };
    assert_eq!(
        describe_context(&status).as_deref(),
        Some("50.0k / 200.0k tokens (25%)")
    );
}

#[test]
fn help_lists_tui_commands_with_their_arguments() {
    let help = describe_help().join("\n");
    assert!(help.contains("/sessions"));
    assert!(help.contains("/compact [focus]"));
    assert!(help.contains("/sandbox start | stop"));
    assert!(help.contains("/select agent [configuration]"));
    assert!(help.contains("/select probe [probe]"));
}

#[tokio::test]
async fn info_breaks_down_tokens_like_the_desktop() {
    let connection = client_shared::ConnectionSettings {
        api_url: "http://127.0.0.1:9".into(),
        client_key: "key".into(),
    };
    let client = psa::api::Client::new(&connection.api_url, &connection.client_key).unwrap();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        crate::tui::network::Network::new(client, events),
        &connection,
        None,
        crate::tui::settings::TuiSettings::default(),
        std::env::temp_dir().join("psa-tui-test-settings.toml"),
    );
    app.open_agent(psa::api::SessionId(1), psa::api::AgentId(2));
    let agent: Agent = serde_json::from_value(json!({
        "id":2,"definition_id":7,"session_id":1,"forked_from":null,"subagent_parent_id":null,
        "name":"worker","hidden":false,"interactive":true,"state":"Idle",
        "usage":{"input_tokens":1200,"output_tokens":300,"total_tokens":1500,"cached_input_tokens":200,"reasoning_output_tokens":100},
        "cacheable_input_tokens":400,
        "next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
    }))
    .unwrap();
    app.view.as_mut().unwrap().agents = vec![agent];

    let text = app.describe_agent(None).join("\n");
    for expected in [
        "Snapshot:    #7",
        "1,200",
        "Cache hit rate         50.0%",
        "Uncached               1,000",
        "Non-thinking             200",
        "  Total                    1,500",
    ] {
        assert!(text.contains(expected), "{expected}:\n{text}");
    }
}
