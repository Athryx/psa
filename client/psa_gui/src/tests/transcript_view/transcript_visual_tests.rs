use super::*;
use std::{collections::HashMap, rc::Rc};

#[component]
fn ReasoningRoot(mode: ReasoningDisplay) -> Element {
    let ui = use_signal(|| {
        let mut settings = DesktopSettings::default();
        settings.preferences.reasoning_display = mode;
        Ui::new(settings)
    });
    use_context_provider(|| ui);
    let kind = psa::api::events::EventType::Thinking(psa::api::events::Thinking {
        contents: llm_api::api::Reasoning {
            summary: vec!["First thought".into(), "Second thought".into()],
            opaque_state: Some(llm_api::api::ProviderData {
                provider: llm_api::providers::ProviderType::OpenAiCodex,
                raw: serde_json::json!({"secret": "opaque-secret"}),
            }),
        },
        metadata: Default::default(),
    });
    rsx! { EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) } }
}

#[test]
fn reasoning_respects_display_mode_and_never_displays_opaque_data() {
    for mode in [
        ReasoningDisplay::Hidden,
        ReasoningDisplay::Collapsed,
        ReasoningDisplay::Shown,
    ] {
        let html = dioxus_ssr::render_element(rsx! { ReasoningRoot { mode } });
        assert!(!html.contains("opaque-secret"), "{html}");
        match mode {
            ReasoningDisplay::Hidden => assert!(!html.contains("First thought"), "{html}"),
            ReasoningDisplay::Collapsed => {
                assert!(
                    html.contains("<details") && !html.contains(" open"),
                    "{html}"
                );
                assert!(
                    html.contains("First thought") && html.contains("Second thought"),
                    "{html}"
                );
            }
            ReasoningDisplay::Shown => {
                assert!(
                    !html.contains("<details") && html.contains("message reasoning"),
                    "{html}"
                );
                assert!(
                    html.contains("First thought") && html.contains("Second thought"),
                    "{html}"
                );
            }
        }
    }
}

#[component]
fn HiddenRowSpacingRoot() -> Element {
    let ui = use_signal(|| {
        let mut settings = DesktopSettings::default();
        settings.preferences.reasoning_display = ReasoningDisplay::Hidden;
        Ui::new(settings)
    });
    use_context_provider(|| ui);
    let message = |text: &str| {
        psa::api::events::EventType::Message(psa::api::events::Message::text(
            psa::api::events::MessageType::Agent,
            text,
        ))
    };
    let thinking = psa::api::events::EventType::Thinking(psa::api::events::Thinking {
        contents: llm_api::api::Reasoning {
            summary: vec!["hidden thought".into()],
            opaque_state: None,
        },
        metadata: Default::default(),
    });
    let results = Rc::new(HashMap::new());
    rsx! {
        div { class: "transcript",
            EventRow { kind: message("before"), provisional: false, results: results.clone() }
            EventRow { kind: thinking, provisional: false, results: results.clone() }
            EventRow { kind: message("after"), provisional: false, results }
        }
    }
}

#[test]
fn hidden_rows_do_not_add_spacing_elements() {
    let html = dioxus_ssr::render_element(rsx! { HiddenRowSpacingRoot {} });
    assert_eq!(html.matches("<article").count(), 2, "{html}");
    assert!(!html.contains("hidden thought"), "{html}");
    assert_eq!(
        html.matches("<div class=\"transcript\"").count(),
        1,
        "{html}"
    );
    assert!(crate::style::CSS.contains(".transcript > * + * { margin-top:12px }"));
}

#[test]
fn tool_calls_render_kwargs_and_separate_highlighted_code() {
    let call = psa::api::events::ToolCall {
        call_id: "call".into(),
        tool_name: "bash".into(),
        arguments: r#"{"command":"if true; then echo hi; fi", "yield_time_ms":10}"#.into(),
        metadata: Default::default(),
    };
    let html = dioxus_ssr::render_element(rsx! {
        EventRow { kind: psa::api::events::EventType::ToolCall(call), provisional: false, results: Rc::new(HashMap::new()) }
    });
    assert!(html.contains("yield_time_ms=10"), "{html}");
    assert!(
        html.contains("syntax-keyword") && html.contains("echo hi"),
        "{html}"
    );
    assert!(!html.contains("&quot;command&quot;"), "{html}");
}

#[test]
fn system_prompt_is_collapsed_and_shows_description() {
    let mut message =
        psa::api::events::Message::text(psa::api::events::MessageType::System, "system body");
    message.description = Some("base instructions".into());
    let kind = psa::api::events::EventType::Message(message);
    let html = dioxus_ssr::render_element(
        rsx! { EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) } },
    );
    assert!(
        html.contains("<details") && !html.contains(" open"),
        "{html}"
    );
    assert!(
        html.contains("System Prompt")
            && html.contains("base instructions")
            && html.contains("system body"),
        "{html}"
    );
}

fn tool_call(id: &str, tool: &str, arguments: serde_json::Value) -> psa::api::events::EventType {
    psa::api::events::EventType::ToolCall(psa::api::events::ToolCall {
        call_id: id.into(),
        tool_name: tool.into(),
        arguments: arguments.to_string(),
        metadata: Default::default(),
    })
}

fn tool_result(id: &str, error: bool) -> psa::api::events::EventType {
    psa::api::events::EventType::ToolResult(psa::api::events::ToolResult {
        delivery: None,
        call_id: id.into(),
        error,
        metadata: Default::default(),
        content: Vec::new(),
    })
}

fn message(role: psa::api::events::MessageType, text: &str) -> psa::api::events::EventType {
    psa::api::events::EventType::Message(psa::api::events::Message::text(role, text))
}

#[test]
fn consecutive_like_rows_form_runs_per_agent_and_compact_rows_close_up() {
    use psa::api::events::MessageType;
    let read = |id: &str| tool_call(id, "read", serde_json::json!({ "path": id }));
    let rows = [
        (AgentId(1), message(MessageType::System, "a")),
        (AgentId(1), message(MessageType::System, "b")),
        (AgentId(1), message(MessageType::Human, "question")),
        (AgentId(1), read("1")),
        (AgentId(1), read("2")),
        (AgentId(1), read("3")),
        (AgentId(2), read("4")),
        (AgentId(2), tool_call("5", "bash", serde_json::json!({}))),
        (AgentId(2), message(MessageType::Agent, "answer")),
        (AgentId(2), read("6")),
    ];
    let preferences = RowPreferences::from(&UiPreferences::default());
    let layouts = layout_rows(
        rows.iter().map(|(agent, kind)| (*agent, kind)),
        &preferences,
    );

    use GroupPosition::*;
    assert_eq!(
        layouts
            .iter()
            .map(|layout| layout.group)
            .collect::<Vec<_>>(),
        [
            First, Last, Single, First, Middle, Last, Single, Single, Single, Single
        ]
    );
    assert_eq!(
        layouts
            .iter()
            .map(|layout| layout.class())
            .collect::<Vec<_>>(),
        [
            "row compact",
            "row compact tight",
            "row",
            "row compact",
            "row compact tight",
            "row compact tight",
            "row compact tight",
            "row compact tight",
            "row",
            "row compact",
        ]
    );
}

#[test]
fn collapsed_code_calls_preview_their_source_and_report_status() {
    let command =
        "for f in *.rs; do\n\n  wc -l \"$f\"\ndone\n".to_owned() + &"echo more\n".repeat(6);
    let call = tool_call(
        "call",
        "bash",
        serde_json::json!({ "command": command, "yield_time_ms": 10 }),
    );
    let render = |results: HashMap<String, psa::api::events::ToolResult>| {
        dioxus_ssr::render_element(rsx! {
            EventRow { kind: call.clone(), provisional: false, results: Rc::new(results) }
        })
    };

    let html = render(HashMap::new());
    let (summary, body) = html.split_once("</summary>").unwrap();
    assert!(
        summary.contains("item-prompt") && summary.contains("wc -l"),
        "{html}"
    );
    assert!(summary.contains("syntax-keyword"), "{html}");
    assert!(summary.contains("+4 more lines"), "{html}");
    assert!(summary.contains("yield_time_ms=10"), "{html}");
    assert!(summary.contains("item-status running"), "{html}");
    assert_eq!(summary.matches("echo more").count(), 2, "{html}");
    assert_eq!(body.matches("echo more").count(), 6, "{html}");

    let psa::api::events::EventType::ToolResult(result) = tool_result("call", true) else {
        unreachable!()
    };
    let html = render(HashMap::from([("call".to_owned(), result)]));
    assert!(
        html.contains("item tool error") && html.contains("item-status failed"),
        "{html}"
    );
    assert!(!html.contains("item-status running"), "{html}");
}

#[component]
fn GroupedTranscriptFixture() -> Element {
    use psa::api::events::MessageType;
    let tab = Tab {
        id: ViewId(1),
        target: TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        },
        kind: TabKind::Transcript,
        preview: false,
        last_focused: 0,
    };
    let target = tab.target;
    let ui = use_signal(move || {
        let mut ui = Ui::new(DesktopSettings::default());
        let mut transcript = Transcript::new(target.session_id, Some(target.agent_id));
        let kinds = [
            message(MessageType::Human, "first question"),
            tool_call("1", "read", serde_json::json!({ "path": "src/a.rs" })),
            tool_result("1", false),
            tool_call(
                "2",
                "read",
                serde_json::json!({ "path": "src/b.rs", "offset": 4 }),
            ),
            tool_result("2", false),
            tool_call("3", "bash", serde_json::json!({ "command": "cargo check" })),
            message(MessageType::Agent, "answer"),
            message(MessageType::Human, "second question"),
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            transcript.apply(StreamMessage::Upsert {
                pending: PendingEvent {
                    id: format!("{index:04}").into(),
                    revision: 1,
                    session_id: target.session_id,
                    agent_id: target.agent_id,
                    kind,
                },
            });
        }
        ui.transcripts.insert(target, transcript.into());
        ui
    });
    use_context_provider(|| ui);
    rsx! { TranscriptView { tab, active: true } }
}

#[test]
fn transcript_groups_tool_runs_and_indexes_user_messages_beside_the_rows() {
    let mut dom = VirtualDom::new(GroupedTranscriptFixture);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);

    // Matched results fold into their calls; the run shares one label.
    assert_eq!(html.matches("data-transcript-row=").count(), 6, "{html}");
    assert_eq!(
        html.matches("class=\"group-label tool\"").count(),
        1,
        "{html}"
    );
    assert_eq!(
        html.matches("class=\"item grouped tool\"").count(),
        2,
        "{html}"
    );
    assert_eq!(html.matches("class=\"item tool\"").count(), 1, "{html}");
    assert!(
        html.contains("<span class=\"item-target\">src/b.rs</span> offset=4"),
        "{html}"
    );
    assert_eq!(
        html.matches("class=\"row compact tight\"").count(),
        2,
        "{html}"
    );
    assert_eq!(html.matches("data-row-gap=\"0\"").count(), 2, "{html}");

    let (rows, navigator) = html.split_once("data-transcript-nav").unwrap();
    assert!(
        rows.contains("transcript-frame") && rows.contains("data-jump-revision=0"),
        "{html}"
    );
    assert_eq!(navigator.matches("data-nav-index=0").count(), 2, "{html}");
    assert_eq!(navigator.matches("data-nav-index=7").count(), 2, "{html}");
    assert!(navigator.contains(">second question</button>"), "{html}");
    assert!(!navigator.contains("answer"), "{html}");
}

fn inference_end(
    input_tokens: Option<u64>,
    outcome: llm_api::api::Outcome,
) -> psa::api::events::EventType {
    psa::api::events::EventType::InferenceResultEnd(psa::api::events::InferenceResultEnd {
        id: 1,
        outcome,
        usage: llm_api::api::Usage {
            input_tokens,
            output_tokens: Some(1_300),
            ..Default::default()
        },
        request_time: std::time::Duration::from_millis(12_400),
        response_id: None,
        provider_metadata: Default::default(),
    })
}

fn inference_start(id: u64) -> psa::api::events::EventType {
    psa::api::events::EventType::InferenceResultStart(psa::api::events::InferenceResultStart {
        id,
        model_config: llm_api::providers::ModelConfig {
            model: llm_api::providers::Model::new(
                llm_api::providers::ProviderType::Openrouter,
                "test",
            )
            .unwrap(),
            effort: llm_api::providers::Effort::High,
        },
        request_config: Default::default(),
    })
}

/// Give an inference end the ID of the inference it closes.
fn paired_end(id: u64, mut end: psa::api::events::EventType) -> psa::api::events::EventType {
    if let psa::api::events::EventType::InferenceResultEnd(end) = &mut end {
        end.id = id;
    }
    end
}

fn completed() -> llm_api::api::Outcome {
    llm_api::api::Outcome::Completed {
        reason: llm_api::api::StopReason::ToolCalls,
    }
}

/// One row as the transcript draws it: a turn's start carries its end.
#[component]
fn DividerPreferenceRoot(
    display: TurnDividerDisplay,
    kind: Shared<psa::api::events::EventType>,
    end: Option<Shared<psa::api::events::EventType>>,
) -> Element {
    let mut preferences = UiPreferences::default();
    preferences.turn_dividers = display;
    rsx! {
        EventContent {
            kind: EventKind::Owned(kind),
            result: end.map(EventKind::Owned),
            preferences: RowPreferences::from(&preferences),
        }
    }
}

#[test]
fn turn_dividers_report_usage_and_follow_the_preference() {
    use TurnDividerDisplay::{Hidden, Labeled, Line};
    let render = |display, kind, end: Option<psa::api::events::EventType>| {
        dioxus_ssr::render_element(rsx! {
            DividerPreferenceRoot { display, kind: Shared::new(kind), end: end.map(Shared::new) }
        })
    };
    let divider = |display, end| render(display, inference_start(1), end);

    let html = divider(Labeled, Some(inference_end(Some(48_230), completed())));
    assert!(html.contains("role=\"separator\""), "{html}");
    assert!(html.contains("48.2k context · 1.3k out · 12.4s"), "{html}");
    // A standalone row has neither a loaded turn number nor a committed turn to open.
    assert!(
        !html.contains("turn-number") && !html.contains("turn-context"),
        "{html}"
    );

    let html = divider(Labeled, Some(inference_end(None, completed())));
    assert!(
        html.contains("1.3k out · 12.4s") && !html.contains("context"),
        "{html}"
    );
    // A turn still running has no figures yet.
    let html = divider(Labeled, None);
    assert!(html.contains("running…") && !html.contains("out"), "{html}");
    assert_eq!(
        divider(Hidden, Some(inference_end(Some(48_230), completed()))),
        ""
    );

    // A plain divider is the rule alone.
    let html = divider(Line, Some(inference_end(Some(48_230), completed())));
    assert!(html.contains("turn-rule"), "{html}");
    assert!(
        !html.contains("context") && !html.contains("12.4s") && !html.contains("turn-label"),
        "{html}"
    );

    // An end draws only its report, whatever the divider preference.
    assert_eq!(
        render(Labeled, inference_end(Some(900), completed()), None),
        ""
    );
    for display in [Labeled, Hidden] {
        let html = render(
            display,
            inference_end(Some(900), llm_api::api::Outcome::Cancelled),
            None,
        );
        assert!(
            html.contains("Inference cancelled") && !html.contains("turn-divider"),
            "{html}"
        );
    }
}

#[test]
fn rows_beside_a_turn_divider_sit_nearer_and_runs_stop_at_it() {
    use psa::api::events::MessageType;
    let read = |id: &str| tool_call(id, "read", serde_json::json!({ "path": id }));
    let rows = [
        (AgentId(1), read("1")),
        (AgentId(1), inference_start(1)),
        (AgentId(1), read("2")),
        (AgentId(1), read("3")),
        (
            AgentId(1),
            inference_end(Some(1), llm_api::api::Outcome::Cancelled),
        ),
        (AgentId(1), message(MessageType::Agent, "answer")),
    ];
    let preferences = RowPreferences::from(&UiPreferences::default());
    let layouts = layout_rows(
        rows.iter().map(|(agent, kind)| (*agent, kind)),
        &preferences,
    );
    assert_eq!(
        layouts
            .iter()
            .map(|layout| layout.class())
            .collect::<Vec<_>>(),
        [
            "row compact",
            "row near",
            "row compact near",
            // The cancelled turn's report is a compact line too.
            "row compact tight",
            "row compact tight",
            "row"
        ]
    );
    assert_eq!(
        layouts
            .iter()
            .map(|layout| layout.spacing.gap_attribute())
            .collect::<Vec<_>>(),
        [None, Some("6"), Some("6"), Some("0"), Some("0"), None]
    );
    use GroupPosition::*;
    assert_eq!(
        layouts
            .iter()
            .map(|layout| layout.group)
            .collect::<Vec<_>>(),
        [Single, Single, First, Last, Single, Single]
    );
}

fn committed(id: u64, kind: psa::api::events::EventType) -> EventEnvelope {
    serde_json::from_value(serde_json::json!({
        "session_id": 1, "agent_id": 1, "position": id,
        "event": {
            "id": id, "source_agent_id": 1, "timestamp": "2026-01-01T00:00:00Z",
            "kind": serde_json::to_value(kind).unwrap(),
        },
    }))
    .unwrap()
}

#[component]
fn TurnTranscriptFixture() -> Element {
    use psa::api::events::MessageType;
    let tab = Tab {
        id: ViewId(1),
        target: TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        },
        kind: TabKind::Transcript,
        preview: false,
        last_focused: 0,
    };
    let target = tab.target;
    let ui = use_signal(move || {
        let mut ui = Ui::new(DesktopSettings::default());
        let mut transcript = Transcript::new(target.session_id, Some(target.agent_id));
        let kinds = [
            message(MessageType::Human, "question"),
            inference_start(1),
            tool_call("1", "read", serde_json::json!({ "path": "src/a.rs" })),
            inference_end(Some(12_000), completed()),
            tool_result("1", false),
            inference_start(2),
            message(MessageType::Agent, "answer"),
            paired_end(2, inference_end(Some(15_500), completed())),
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            transcript.apply(StreamMessage::Committed {
                event: committed(index as u64 + 1, kind),
                pending_id: None,
            });
        }
        ui.transcripts.insert(target, transcript.into());
        ui
    });
    use_context_provider(|| ui);
    rsx! { TranscriptView { tab, active: true } }
}

#[component]
fn RowMenuRoot(turn: Option<u64>) -> Element {
    let ui = use_signal(|| Ui::new(DesktopSettings::default()));
    use_context_provider(|| ui);
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(1),
    };
    let menu = RowMenu {
        x: 12.0,
        y: 34.0,
        turn,
    };
    rsx! { RowContextMenu { menu, target, onclose: |_| {} } }
}

#[test]
fn row_menu_offers_context_only_once_the_turn_has_ended() {
    let html = dioxus_ssr::render_element(rsx! { RowMenuRoot { turn: Some(8) } });
    assert!(html.contains("Show context"), "{html}");
    assert!(html.contains("left:min(12px"), "{html}");
    assert!(!html.contains("disabled"), "{html}");

    let html = dioxus_ssr::render_element(rsx! { RowMenuRoot { turn: None } });
    assert!(html.contains("disabled"), "{html}");
}

#[test]
fn committed_turns_are_numbered_and_offer_their_context() {
    let mut dom = VirtualDom::new(TurnTranscriptFixture);
    dom.rebuild_in_place();
    let html = dioxus_ssr::render(&dom);
    assert_eq!(html.matches("class=\"turn-divider\"").count(), 2, "{html}");
    assert_eq!(html.matches("class=\"turn-context\"").count(), 2, "{html}");
    // Each divider opens its turn: what it was sent above, its output below.
    let question = html.find("<pre>question</pre>").unwrap();
    let first = html.find("turn 1</span> · 12.0k context").expect(&html);
    let call = html.find("src/a.rs").unwrap();
    let second = html.find("turn 2</span> · 15.5k context").expect(&html);
    let answer = html.find("<p>answer</p>").unwrap();
    assert!(
        [question, first, call, second, answer].is_sorted(),
        "{html}"
    );
    assert_eq!(html.matches("data-row-gap=\"6\"").count(), 4, "{html}");
}
