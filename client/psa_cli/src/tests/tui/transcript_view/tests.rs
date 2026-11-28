use super::*;
use llm_api::{
    api::{HistoryItemMetadata, RequestConfig, StopReason, TextBlock, Usage},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa::api::{
    AgentId, EventEnvelope, SessionId, StreamMessage,
    events::{
        Event, EventId, InferenceResultEnd, InferenceResultStart, Log, Message, Thinking,
        ToolResult,
    },
};

fn commit(transcript: &mut Transcript, id: u64, kind: EventType) {
    transcript.apply(StreamMessage::Committed {
        event: EventEnvelope {
            session_id: SessionId(1),
            agent_id: AgentId(1),
            position: id,
            event: Event {
                id: EventId(id),
                source_agent_id: AgentId(1),
                timestamp: time::OffsetDateTime::UNIX_EPOCH,
                kind,
            },
        },
        pending_id: None,
    });
}

fn render_with(
    transcript: &Transcript,
    preferences: &TuiPreferences,
    details: bool,
) -> Vec<String> {
    let palette = crate::tui::theme::Theme::Dark.palette();
    let options = DisplayOptions {
        preferences,
        palette: &palette,
        details,
    };
    build_lines(transcript, &options, 40)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

/// Default preferences without turn dividers, or the details view.
fn render(transcript: &Transcript, details: bool) -> Vec<String> {
    let preferences = TuiPreferences {
        turn_dividers: TurnDividerDisplay::Hidden,
        ..TuiPreferences::default()
    };
    let preferences = if details {
        preferences.with_details()
    } else {
        preferences
    };
    render_with(transcript, &preferences, details)
}

fn tool_call(id: &str, arguments: &str) -> EventType {
    EventType::ToolCall(ToolCall {
        call_id: id.into(),
        tool_name: "bash".into(),
        arguments: arguments.into(),
        metadata: HistoryItemMetadata::default(),
    })
}

fn tool_result(id: &str, output: &str, error: bool) -> EventType {
    EventType::ToolResult(ToolResult {
        call_id: id.into(),
        content: vec![ContentBlock::Text(TextBlock {
            text: output.into(),
            annotations: Vec::new(),
        })],
        error,
        metadata: HistoryItemMetadata::default(),
        delivery: None,
    })
}

#[test]
fn messages_get_markers_and_wrap_under_them() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(
        &mut transcript,
        1,
        EventType::Message(Message::text(MessageType::Human, "hi")),
    );
    commit(
        &mut transcript,
        2,
        EventType::Message(Message::text(
            MessageType::Agent,
            "a reply that is long enough to wrap onto a second line",
        )),
    );
    let lines = render(&transcript, false);
    assert_eq!(lines[0], "› hi");
    assert_eq!(lines[1], "");
    assert!(lines[2].starts_with("• a reply"));
    assert!(lines[3].starts_with("  "));
}

#[test]
fn tool_calls_show_the_key_argument_and_a_result_preview() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(
        &mut transcript,
        1,
        tool_call("c1", r#"{"command":"ls -la","timeout":5}"#),
    );
    commit(
        &mut transcript,
        2,
        tool_result("c1", "a\nb\nc\nd\ne", false),
    );
    commit(
        &mut transcript,
        3,
        tool_call("c2", r#"{"command":"sleep 9"}"#),
    );

    let lines = render(&transcript, false);
    assert_eq!(
        lines,
        vec![
            "● bash ls -la",
            "  a",
            "  b",
            "  c",
            "  … 2 more lines",
            "",
            "● bash sleep 9",
            "  running…",
        ]
    );

    let detailed = render(&transcript, true);
    assert!(detailed.iter().any(|line| line.contains("\"timeout\":5")));
    assert!(detailed.contains(&"  e".to_owned()));
}

#[test]
fn harness_messages_and_logs_follow_their_settings() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(
        &mut transcript,
        1,
        EventType::Message(Message::harness("nudge text", "Nudge")),
    );
    commit(
        &mut transcript,
        2,
        EventType::Log(Log {
            error: false,
            message: "note".into(),
        }),
    );
    commit(
        &mut transcript,
        3,
        EventType::Log(Log {
            error: true,
            message: "broken".into(),
        }),
    );
    assert_eq!(
        render(&transcript, false),
        vec!["· Nudge", "  nudge text", "", "✗ broken"],
        "harness messages show by default, info logs do not"
    );
    let quiet = TuiPreferences {
        show_harness_messages: false,
        turn_dividers: TurnDividerDisplay::Hidden,
        ..TuiPreferences::default()
    };
    assert_eq!(render_with(&transcript, &quiet, false), vec!["✗ broken"]);
    let detailed = render(&transcript, true).join("\n");
    assert!(detailed.contains("Nudge") && detailed.contains("note"));
}

fn thinking(summary: &str) -> EventType {
    EventType::Thinking(Thinking {
        contents: llm_api::api::Reasoning {
            summary: vec![summary.into()],
            opaque_state: None,
        },
        metadata: HistoryItemMetadata::default(),
    })
}

#[test]
fn reasoning_follows_its_setting() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(&mut transcript, 1, thinking("first idea\nsecond idea"));
    let mut preferences = TuiPreferences {
        turn_dividers: TurnDividerDisplay::Hidden,
        ..TuiPreferences::default()
    };
    assert_eq!(
        render_with(&transcript, &preferences, false),
        vec!["∴ first idea…"]
    );
    preferences.reasoning_display = ReasoningDisplay::Shown;
    assert_eq!(
        render_with(&transcript, &preferences, false),
        vec!["∴ first idea", "  second idea"]
    );
    preferences.reasoning_display = ReasoningDisplay::Hidden;
    assert!(render_with(&transcript, &preferences, false).is_empty());
}

#[test]
fn turn_dividers_carry_the_turn_number_and_usage() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    let config = ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test/model").unwrap(),
        effort: Effort::Medium,
    };
    commit(
        &mut transcript,
        1,
        EventType::InferenceResultStart(InferenceResultStart {
            id: 7,
            model_config: config,
            request_config: RequestConfig::default(),
        }),
    );
    commit(
        &mut transcript,
        2,
        EventType::InferenceResultEnd(InferenceResultEnd {
            id: 7,
            outcome: Outcome::Completed {
                reason: StopReason::EndTurn,
            },
            usage: Usage {
                input_tokens: Some(1500),
                output_tokens: Some(20),
                ..Usage::default()
            },
            request_time: std::time::Duration::from_millis(2500),
            response_id: None,
            provider_metadata: Default::default(),
        }),
    );

    let labeled = render_with(&transcript, &TuiPreferences::default(), false);
    assert_eq!(labeled.len(), 1);
    assert!(
        labeled[0].starts_with("  ── turn 1 · 1.5k in · 20 out · 2.5s ──"),
        "{labeled:?}"
    );
    assert_eq!(labeled[0].chars().count(), 40);

    let line = TuiPreferences {
        turn_dividers: TurnDividerDisplay::Line,
        ..TuiPreferences::default()
    };
    assert_eq!(
        render_with(&transcript, &line, false),
        vec![format!("  {}", "─".repeat(38))]
    );
}

#[test]
fn text_results_and_tool_output_follow_their_settings() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(
        &mut transcript,
        1,
        EventType::Result(AgentResult::Text("done".into())),
    );
    commit(&mut transcript, 2, tool_call("c1", r#"{"command":"ls"}"#));
    commit(&mut transcript, 3, tool_result("c1", "file", false));
    commit(
        &mut transcript,
        4,
        tool_call("c2", r#"{"command":"false"}"#),
    );
    commit(&mut transcript, 5, tool_result("c2", "failed", true));

    let preferences = TuiPreferences {
        turn_dividers: TurnDividerDisplay::Hidden,
        tool_output_lines: 0,
        ..TuiPreferences::default()
    };
    assert_eq!(
        render_with(&transcript, &preferences, false),
        vec!["● bash ls", "", "● bash false", "  failed"],
        "text results hidden; errors shown even without output lines"
    );

    let shown = TuiPreferences {
        hide_text_recorded_results: false,
        ..preferences
    };
    assert!(render_with(&transcript, &shown, false).contains(&"  done".to_owned()));
}

#[test]
fn system_prompts_show_as_a_label_by_default() {
    let mut transcript = Transcript::new(SessionId(1), Some(AgentId(1)));
    commit(
        &mut transcript,
        1,
        EventType::Message(Message::system("Be brief.", "Base prompt")),
    );
    let with = |system_prompts| TuiPreferences {
        system_prompts,
        turn_dividers: TurnDividerDisplay::Hidden,
        ..TuiPreferences::default()
    };
    assert_eq!(
        render(&transcript, false),
        vec!["· System prompt · Base prompt"]
    );
    assert!(render_with(&transcript, &with(SystemPromptDisplay::Hidden), false).is_empty());
    assert_eq!(
        render_with(&transcript, &with(SystemPromptDisplay::Full), false),
        vec!["· System prompt · Base prompt", "  Be brief."]
    );
    assert!(
        render(&transcript, true).contains(&"  Be brief.".to_owned()),
        "Ctrl+O shows it in full"
    );
}
