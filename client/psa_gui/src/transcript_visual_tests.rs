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
    let kind = psa_api::events::EventType::Thinking(psa_api::events::Thinking {
        contents: llm_api::api::Reasoning {
            summary: vec!["First thought".into(), "Second thought".into()],
            opaque_state: Some(llm_api::api::ProviderOpaqueData {
                provider: llm_api::providers::ProviderType::OpenAiCodex,
                data: serde_json::json!({"secret": "opaque-secret"}),
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
        psa_api::events::EventType::Message(psa_api::events::Message::text(
            psa_api::events::MessageType::Agent,
            text,
        ))
    };
    let thinking = psa_api::events::EventType::Thinking(psa_api::events::Thinking {
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
    let call = psa_api::events::ToolCall {
        call_id: "call".into(),
        tool_name: "bash".into(),
        arguments: r#"{"command":"if true; then echo hi; fi", "yield_time_ms":10}"#.into(),
        metadata: Default::default(),
    };
    let html = dioxus_ssr::render_element(rsx! {
        EventRow { kind: psa_api::events::EventType::ToolCall(call), provisional: false, results: Rc::new(HashMap::new()) }
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
        psa_api::events::Message::text(psa_api::events::MessageType::System, "system body");
    message.description = Some("base instructions".into());
    let kind = psa_api::events::EventType::Message(message);
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

fn tool_call(id: &str, tool: &str, arguments: serde_json::Value) -> psa_api::events::EventType {
    psa_api::events::EventType::ToolCall(psa_api::events::ToolCall {
        call_id: id.into(),
        tool_name: tool.into(),
        arguments: arguments.to_string(),
        metadata: Default::default(),
    })
}

fn tool_result(id: &str, error: bool) -> psa_api::events::EventType {
    psa_api::events::EventType::ToolResult(psa_api::events::ToolResult {
        call_id: id.into(),
        error,
        metadata: Default::default(),
        content: Vec::new(),
    })
}

fn message(role: psa_api::events::MessageType, text: &str) -> psa_api::events::EventType {
    psa_api::events::EventType::Message(psa_api::events::Message::text(role, text))
}

#[test]
fn consecutive_like_rows_form_runs_per_agent_and_compact_rows_close_up() {
    use psa_api::events::MessageType;
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
    let render = |results: HashMap<String, psa_api::events::ToolResult>| {
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

    let psa_api::events::EventType::ToolResult(result) = tool_result("call", true) else {
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
    use psa_api::events::MessageType;
    let tab = Tab {
        id: ViewId(1),
        target: TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(1),
        },
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
                    id: format!("{index:04}"),
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
