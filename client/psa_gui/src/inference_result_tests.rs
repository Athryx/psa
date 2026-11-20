use super::*;
use llm_api::{
    Error,
    api::{Outcome, StopReason, Usage},
};
use psa::api::events::{EventType, InferenceResultEnd};
use std::{collections::HashMap, rc::Rc, time::Duration};

fn render_outcome(outcome: Outcome) -> String {
    let kind = EventType::InferenceResultEnd(InferenceResultEnd {
        id: 42,
        outcome,
        usage: Usage::default(),
        request_time: Duration::ZERO,
        response_id: None,
        provider_metadata: Default::default(),
    });
    dioxus_ssr::render_element(rsx! {
        EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) }
    })
}

#[test]
fn failed_inference_displays_protocol_http_and_transport_errors() {
    for (error, expected) in [
        (
            Error::Protocol {
                message: "choice received after finish_reason".into(),
            },
            "stream protocol error: choice received after finish_reason",
        ),
        (
            Error::Http {
                status: 429,
                message: "Too many requests\nTry again later".into(),
                retry_after_seconds: Some(10),
            },
            "HTTP 429: Too many requests\nTry again later",
        ),
        (
            Error::Transport {
                message: "Connection reset".into(),
            },
            "transport error: Connection reset",
        ),
    ] {
        let html = render_outcome(Outcome::Failed(error));
        assert!(html.contains("Inference failed"), "{html}");
        assert!(html.contains("class=\"item error\""), "{html}");
        assert!(html.contains(">FAILED<"), "{html}");
        assert!(html.contains("role=\"alert\""), "{html}");
        assert!(html.contains(expected), "{html}");
    }
}

#[test]
fn failed_inference_retains_provider_code_and_details_as_escaped_text() {
    let html = render_outcome(Outcome::Failed(Error::Provider {
        code: Some("quota_exceeded".into()),
        message: "Quota exhausted <script>alert(1)</script>".into(),
        metadata: [("request_id".into(), serde_json::json!("request-123"))].into(),
    }));
    assert!(html.contains("provider error: Quota exhausted"), "{html}");
    assert!(html.contains("quota_exceeded"), "{html}");
    assert!(html.contains("request-123"), "{html}");
    assert!(html.contains("Error details"), "{html}");
    assert!(html.contains("&#60;script&#62;"), "{html}");
    assert!(!html.contains("<script>"), "{html}");
}

#[test]
fn incomplete_inference_displays_reason() {
    for (reason, expected) in [
        (StopReason::TokenLimit, "Token limit reached"),
        (StopReason::ContentFilter, "Content filtered"),
        (
            StopReason::Other("Provider stopped early".into()),
            "Provider stopped early",
        ),
    ] {
        let html = render_outcome(Outcome::Incomplete { reason });
        assert!(html.contains("Inference incomplete"), "{html}");
        assert!(html.contains("class=\"item item-line warning\""), "{html}");
        assert!(html.contains(expected), "{html}");
        assert!(!html.contains("Inference failed"), "{html}");
    }
}

#[test]
fn cancelled_inference_is_visible_without_failure_styling() {
    let html = render_outcome(Outcome::Cancelled);
    assert!(html.contains("Inference cancelled"), "{html}");
    assert!(html.contains("class=\"item item-line\""), "{html}");
    assert!(html.contains(">CANCELLED<"), "{html}");
    assert!(!html.contains("role=\"alert\""), "{html}");
}

#[test]
fn successful_or_in_progress_inference_does_not_add_a_row() {
    for outcome in [
        Outcome::Completed {
            reason: StopReason::EndTurn,
        },
        Outcome::InProgress,
    ] {
        let html = render_outcome(outcome);
        assert!(!html.contains("<article"), "{html}");
        assert!(!html.contains("Inference"), "{html}");
    }
}
