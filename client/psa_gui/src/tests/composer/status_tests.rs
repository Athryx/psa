use super::*;

fn status_fixture() -> Element {
    rsx! { ComposerStatus {
        path: Some("/home/project".to_string()),
        model: Some("Example Model".to_string()),
        effort: Some(Effort::High),
        context_tokens: Some(46_000),
        context_window: Some(200_000),
    } }
}

#[test]
fn status_displays_path_model_effort_and_usage() {
    let mut dom = VirtualDom::new(status_fixture);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/home/project"), "{html}");
    assert!(html.contains("Example Model"), "{html}");
    assert!(html.contains("class=\"effort-high\">high"), "{html}");
    assert!(html.contains("(23% / 200k)"), "{html}");
    assert!(html.contains("Last inference"), "{html}");
}

fn agent_composer_fixture() -> Element {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: AgentId(2),
    };
    let ui =
        use_signal(|| {
            let mut state = Ui::new(DesktopSettings::default());
            state.sessions.push(serde_json::from_value(serde_json::json!({
            "id":1,"forked_from":null,"name":"test","working_directory":"/home/project",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":"openrouter/default","effort":"Low"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap());
            state.models.push(
                serde_json::from_value(serde_json::json!({
                    "model":"openrouter/actual","name":"Actual Model",
                    "input_modalities":[],"context_window":200000,"max_output_tokens":null,
                    "cost":{"tiers":[]},"reasoning":true,"reasoning_options":[],
                    "authed":true,"pinned":false,"refreshed_at":"2024-01-01T00:00:00Z"
                }))
                .unwrap(),
            );
            let mut transcript = Transcript::new(target.session_id, Some(target.agent_id));
            transcript.apply(StreamMessage::Committed {
                event: serde_json::from_value(serde_json::json!({
                    "session_id":1,"agent_id":2,"position":1,
                    "event":{"id":1,"source_agent_id":2,"timestamp":"2026-01-01T00:00:00Z",
                        "kind":{"Model":{"model":"openrouter/actual","effort":"High"}}}
                }))
                .unwrap(),
                pending_id: None,
            });
            state.transcripts.insert(target, transcript.into());
            state
        });
    use_context_provider(|| ui);
    rsx! { Composer { tab: Tab { id: ViewId(1), target, kind: TabKind::Transcript, preview: false, last_focused: 0 } } }
}

#[test]
fn composer_uses_agent_model_not_session_default() {
    let mut dom = VirtualDom::new(agent_composer_fixture);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("/home/project"), "{html}");
    assert!(html.contains("Actual Model"), "{html}");
    assert!(html.contains("class=\"effort-high\">high"), "{html}");
    assert!(html.contains("(? / 200k)"), "{html}");
    assert!(!html.contains("class=\"effort-low\""), "{html}");
}

#[test]
fn draft_status_resolves_selected_policy_without_assuming_an_unselected_model() {
    let session: ModelConfig = serde_json::from_value(serde_json::json!({
        "model":"openrouter/default","effort":"Low"
    }))
    .unwrap();
    for selection in [ModelSelection::Global, ModelSelection::Parent] {
        assert_eq!(
            draft_status_model(Some(&selection), Some(&session)),
            (Some(session.model.clone()), Some(Effort::Low))
        );
    }
    for effort in [
        EffortSelection::Global,
        EffortSelection::Parent,
        EffortSelection::Fixed(Effort::High),
    ] {
        let expected = if effort == EffortSelection::Fixed(Effort::High) {
            Effort::High
        } else {
            Effort::Low
        };
        let selection = ModelSelection::Fixed {
            model: "openrouter/vendor/actual".parse().unwrap(),
            effort,
        };
        let (model, effort) = draft_status_model(Some(&selection), Some(&session));
        assert_eq!(model.unwrap().api_name, "vendor/actual");
        assert_eq!(effort, Some(expected));
    }
    assert_eq!(draft_status_model(None, Some(&session)), (None, None));
    assert_eq!(
        draft_status_model(Some(&ModelSelection::Global), None),
        (None, None)
    );
    let fixed = ModelSelection::Fixed {
        model: "openrouter/actual".parse().unwrap(),
        effort: EffortSelection::Fixed(Effort::Max),
    };
    let (model, effort) = draft_status_model(Some(&fixed), None);
    assert_eq!(model.unwrap().api_name, "actual");
    assert_eq!(effort, Some(Effort::Max));
}

fn draft_status_fixture() -> Element {
    let target = TranscriptTarget {
        session_id: SessionId(1),
        agent_id: crate::app::DRAFT_ROOT,
    };
    let ui =
        use_signal(|| {
            let mut state = Ui::new(DesktopSettings::default());
            state.sessions.push(serde_json::from_value(serde_json::json!({
            "id":1,"forked_from":null,"name":"test","working_directory":"/tmp",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":"openrouter/default","effort":"Low"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap());
            state.models.push(
                serde_json::from_value(serde_json::json!({
                    "model":"openrouter/actual","name":"Draft Model",
                    "input_modalities":[],"context_window":200000,"max_output_tokens":null,
                    "cost":{"tiers":[]},"reasoning":true,"reasoning_options":[],
                    "authed":true,"pinned":false,"refreshed_at":"2024-01-01T00:00:00Z"
                }))
                .unwrap(),
            );
            state.draft_roots.insert(
                SessionId(1),
                crate::app::DraftRoot {
                    model: Some(ModelSelection::Fixed {
                        model: "openrouter/actual".parse().unwrap(),
                        effort: EffortSelection::Fixed(Effort::High),
                    }),
                    ready: true,
                    ..Default::default()
                }
                .into(),
            );
            state
        });
    use_context_provider(|| ui);
    rsx! { DraftComposer { tab: Tab { id: ViewId(1), target, kind: TabKind::Transcript, preview: false, last_focused: 0 } } }
}

#[test]
fn draft_composer_displays_selected_model_and_unknown_usage() {
    let mut dom = VirtualDom::new(draft_status_fixture);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Draft Model"), "{html}");
    assert!(html.contains("class=\"effort-high\">high"), "{html}");
    assert!(html.contains("(? / 200k)"), "{html}");
}

#[test]
fn context_usage_handles_unknown_and_zero_without_inventing_usage() {
    assert_eq!(format_context_usage(None, Some(200_000)), "(? / 200k)");
    assert_eq!(format_context_usage(None, Some(499_499)), "(? / 499k)");
    assert_eq!(format_context_usage(None, Some(499_500)), "(? / 500k)");
    assert_eq!(format_context_usage(None, Some(1_048_576)), "(? / 1049k)");
    assert_eq!(format_context_usage(Some(0), Some(200_000)), "(0% / 200k)");
    assert_eq!(format_context_usage(Some(100), Some(0)), "(? / ?)");
    assert_eq!(format_context_usage(None, None), "(? / ?)");
    assert_eq!(
        format_context_usage(Some(u64::MAX), Some(1)),
        format!("({}% / 1)", u128::from(u64::MAX) * 100)
    );
}
