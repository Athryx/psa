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
