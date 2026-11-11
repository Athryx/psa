use super::*;
use llm_api::{
    api::{ImageBlock, MediaSource, Reasoning, RequestConfig, StopReason},
    providers::{Effort, Model, ProviderType},
};
use psa_api::{AgentId, ToolId, events::*};
use std::time::Duration;

fn events(kinds: Vec<EventType>) -> Vec<Event> {
    kinds
        .into_iter()
        .enumerate()
        .map(|(index, kind)| Event {
            id: EventId(index as u64 + 1),
            source_agent_id: AgentId(1),
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        })
        .collect()
}

fn model(name: &str) -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, name).unwrap(),
        effort: Effort::High,
    }
}

fn message(role: MessageType, text: &str) -> EventType {
    EventType::Message(Message::text(role, text))
}

fn start(id: u64) -> EventType {
    EventType::InferenceResultStart(InferenceResultStart {
        id,
        model_config: model("test"),
        request_config: RequestConfig::default(),
    })
}

fn end(id: u64, total: u64) -> EventType {
    EventType::InferenceResultEnd(InferenceResultEnd {
        id,
        outcome: Outcome::Completed {
            reason: StopReason::EndTurn,
        },
        usage: Usage {
            total_tokens: Some(total),
            ..Default::default()
        },
        request_time: Duration::ZERO,
        response_id: None,
        provider_metadata: Default::default(),
    })
}

fn call(id: &str, name: &str, args: &str) -> EventType {
    EventType::ToolCall(ToolCall {
        call_id: id.into(),
        tool_name: name.into(),
        arguments: args.into(),
        metadata: Default::default(),
    })
}

fn result(id: &str, text: &str) -> EventType {
    EventType::ToolResult(ToolResult {
        delivery: None,
        call_id: id.into(),
        content: crate::prompts::text(text),
        error: false,
        metadata: Default::default(),
    })
}

fn summary(text: &str) -> EventType {
    EventType::Compaction(Compaction {
        summary: CompactionSummary::Text(text.into()),
        usage: Usage::default(),
    })
}

fn thinking(text: &str) -> EventType {
    EventType::Thinking(Thinking {
        contents: Reasoning {
            summary: vec![text.into()],
            opaque_state: None,
        },
        metadata: Default::default(),
    })
}

#[test]
fn does_nothing_without_discardable_conversation() {
    assert!(prepare(&[], 1).is_none());
    assert!(prepare(&events(vec![summary("checkpoint")]), 1).is_none());
    let transcript = events(vec![message(MessageType::Human, "hello")]);
    assert!(prepare(&transcript, 1).is_none());
    assert!(prepare(&transcript, u64::MAX).is_none());
    assert!(
        prepare(
            &events(vec![
                message(MessageType::System, "hidden"),
                message(MessageType::Human, "hello")
            ]),
            0
        )
        .is_none()
    );
}

#[test]
fn zero_budget_summarizes_short_conversations_but_keeps_the_latest_entry() {
    let transcript = events(vec![
        message(MessageType::Human, "request"),
        start(1),
        message(MessageType::Agent, "answer"),
        end(1, 10),
    ]);
    assert!(prepare(&transcript, u64::MAX).is_none());
    let preparation = prepare(&transcript, 0).unwrap();
    assert!(
        preparation
            .turn_prefix_prompt
            .unwrap()
            .contains("[User]: request")
    );
    assert!(preparation.retained_events.iter().any(|event| matches!(
        &event.kind,
        EventType::Message(message) if message.message_type == MessageType::Agent
    )));
}

#[test]
fn image_only_discarded_messages_still_produce_a_summary_request() {
    let image = EventType::Message(Message {
        message_type: MessageType::Human,
        source: None,
        description: None,
        metadata: Default::default(),
        content: vec![ContentBlock::Image(ImageBlock {
            source: MediaSource::Url("image".into()),
            detail: None,
        })],
    });
    let preparation =
        prepare(&events(vec![image, message(MessageType::Human, "keep")]), 1).unwrap();
    assert!(
        preparation
            .history_prompt
            .unwrap()
            .starts_with("<conversation>\n\n</conversation>")
    );
}

#[test]
fn cuts_at_user_boundary_and_preserves_timestamp() {
    let transcript = events(vec![
        message(MessageType::Human, "old request"),
        start(1),
        message(MessageType::Agent, "old answer"),
        end(1, 100),
        message(MessageType::Human, "new request"),
    ]);
    let preparation = prepare(&transcript, 1).unwrap();
    let prompt = preparation.history_prompt.unwrap();
    assert!(prompt.contains("[User]: old request\n\n[Assistant]: old answer"));
    assert!(!prompt.contains("new request"));
    assert!(preparation.turn_prefix_prompt.is_none());
    assert_eq!(preparation.retained_events.len(), 1);
    assert_eq!(
        preparation.retained_events[0].timestamp,
        transcript[4].timestamp
    );
    assert_eq!(preparation.tokens_before, 103);
}

#[test]
fn split_turn_uses_separate_history_and_prefix_requests() {
    let transcript = events(vec![
        message(MessageType::Human, "old task"),
        message(MessageType::Agent, "done"),
        message(MessageType::Human, "current task"),
        start(1),
        message(MessageType::Agent, "first step"),
        end(1, 0),
        start(2),
        message(MessageType::Agent, "continue here"),
        end(2, 0),
    ]);
    let preparation = prepare(&transcript, 1).unwrap();
    let history = preparation.history_prompt.as_ref().unwrap();
    let prefix = preparation.turn_prefix_prompt.as_ref().unwrap();
    assert!(history.contains("old task"));
    assert!(!history.contains("current task"));
    assert!(prefix.contains("[User]: current task\n\n[Assistant]: first step"));
    assert!(!prefix.contains("continue here"));
    assert_eq!(preparation.retained_events.len(), 1);
    assert_eq!(
        preparation.finish_summary(Some("history"), Some("prefix")),
        "history\n\n---\n\n**Turn Context (split turn):**\n\nprefix"
    );
}

#[test]
fn split_only_keeps_previous_summary_without_a_redundant_history_request() {
    let transcript = events(vec![
        summary("previous"),
        message(MessageType::Human, "request"),
        start(1),
        message(MessageType::Agent, "continued"),
        end(1, 0),
    ]);
    let preparation = prepare(&transcript, 1).unwrap();
    assert!(preparation.history_prompt.is_none());
    assert!(preparation.turn_prefix_prompt.is_some());
    assert_eq!(preparation.previous_summary.as_deref(), Some("previous"));
    assert_eq!(
        preparation.finish_summary(None, Some("prefix")),
        "previous\n\n---\n\n**Turn Context (split turn):**\n\nprefix"
    );

    let preparation = prepare(
        &events(vec![
            message(MessageType::Human, "request"),
            start(1),
            message(MessageType::Agent, "continued"),
            end(1, 0),
        ]),
        1,
    )
    .unwrap();
    assert!(
        preparation
            .finish_summary(None, Some("prefix"))
            .starts_with("No prior history.")
    );
}

#[test]
fn trailing_tool_results_keep_their_entire_parallel_response() {
    let transcript = events(vec![
        message(MessageType::Human, "request"),
        start(1),
        thinking("private"),
        message(MessageType::Agent, "reading"),
        call("a", "read", r#"{"path":"a"}"#),
        call("b", "read", r#"{"path":"b"}"#),
        end(1, 0),
        result("b", &"b".repeat(10000)),
        result("a", &"a".repeat(10000)),
    ]);
    let preparation = prepare(&transcript, 1).unwrap();
    assert_eq!(preparation.retained_events.len(), 6);
    assert!(matches!(
        preparation.retained_events[0].kind,
        EventType::Thinking(_)
    ));
    assert!(
        matches!(&preparation.retained_events[5].kind, EventType::ToolResult(result) if result.call_id == "a")
    );
    assert!(
        preparation
            .turn_prefix_prompt
            .unwrap()
            .contains("[User]: request")
    );
}

#[test]
fn every_budget_preserves_call_result_pairs_and_whole_inference_output() {
    let transcript = events(vec![
        message(MessageType::Human, "request"),
        start(1),
        thinking("secret"),
        message(MessageType::Agent, "one"),
        call("a", "read", r#"{"path":"a"}"#),
        end(1, 0),
        result("a", "output"),
        start(2),
        thinking("thoughts"),
        message(MessageType::Agent, "two"),
        end(2, 0),
        message(MessageType::Human, "next request"),
    ]);
    for budget in 0..100 {
        let Some(preparation) = prepare(&transcript, budget) else {
            continue;
        };
        let calls = preparation
            .retained_events
            .iter()
            .filter(|event| matches!(event.kind, EventType::ToolCall(_)))
            .count();
        let results = preparation
            .retained_events
            .iter()
            .filter(|event| matches!(event.kind, EventType::ToolResult(_)))
            .count();
        assert_eq!(calls, results, "budget {budget}");
        let retained_text = serialize_conversation(&events(
            preparation
                .retained_events
                .iter()
                .map(|event| event.kind.clone())
                .collect(),
        ));
        for (thought, text) in [("secret", "one"), ("thoughts", "two")] {
            let has_thought = preparation.retained_events.iter().any(|event| {
                matches!(&event.kind,
                EventType::Thinking(thinking) if thinking.contents.summary == [thought])
            });
            assert_eq!(
                has_thought,
                retained_text.contains(&format!("[Assistant]: {text}")),
                "budget {budget}"
            );
        }
    }
}

#[test]
fn repeated_compaction_keeps_naked_assistant_runs_atomic() {
    let transcript = events(vec![
        summary("prior"),
        message(MessageType::Human, "request"),
        thinking("reasoning"),
        message(MessageType::Agent, "working"),
        call("a", "read", r#"{"path":"a"}"#),
        call("b", "read", r#"{"path":"b"}"#),
        result("a", "output"),
        result("b", "output"),
        message(MessageType::Agent, "continued"),
        message(MessageType::Human, "next"),
    ]);
    assert_eq!(
        find_cut_points(&transcript[1..]).unwrap(),
        [(0, true), (1, false), (7, false), (8, true)]
    );
    for budget in 0..100 {
        let Some(preparation) = prepare(&transcript, budget) else {
            continue;
        };
        let kinds = preparation
            .retained_events
            .iter()
            .map(|event| &event.kind)
            .collect::<Vec<_>>();
        let has_working = kinds.iter().any(|kind| {
            matches!(kind, EventType::Message(message)
            if text_content(&message.content, "") == "working")
        });
        assert_eq!(
            has_working,
            kinds
                .iter()
                .any(|kind| matches!(kind, EventType::Thinking(_)))
        );
        assert_eq!(
            has_working,
            kinds
                .iter()
                .any(|kind| matches!(kind, EventType::ToolCall(_)))
        );
        assert_eq!(
            has_working,
            kinds
                .iter()
                .any(|kind| matches!(kind, EventType::ToolResult(_)))
        );
    }
}

#[test]
fn unclosed_or_invalid_groups_are_not_compacted() {
    let cases = vec![
        vec![start(1), message(MessageType::Agent, "partial")],
        vec![start(1), start(2), end(2, 0)],
        vec![start(1), end(2, 0)],
        vec![end(1, 0)],
        vec![call("a", "read", "{}")],
        vec![result("orphan", "output")],
        vec![
            start(1),
            call("a", "read", "{}"),
            result("a", "output"),
            end(1, 0),
        ],
    ];
    for mut kinds in cases {
        kinds.insert(0, message(MessageType::Human, "old"));
        kinds.push(message(MessageType::Human, "new"));
        assert!(prepare(&events(kinds), 1).is_none());
    }
}

#[test]
fn snapshots_and_control_are_not_serialized_or_retained_but_model_changes_are() {
    let transcript = events(vec![
        EventType::Model(model("first")),
        message(MessageType::System, "secret system"),
        message(MessageType::Human, "old"),
        message(MessageType::Human, "request"),
        EventType::Model(model("second")),
        message(MessageType::System, "new system"),
        EventType::ToolDefinition(ToolDefinition {
            id: ToolId::Read,
            definition: crate::components::tools::tool_definition(ToolId::Read).unwrap(),
        }),
        EventType::Log(Log {
            error: false,
            message: "log".into(),
        }),
        EventType::Result(psa_api::AgentResult::Text("artifact".into())),
        message(MessageType::Agent, "answer"),
    ]);
    // Keep from the second user message, including its subsequent model transition.
    let estimates = token_estimates(&transcript);
    let budget = estimates[3..].iter().sum();
    let preparation = prepare(&transcript, budget).unwrap();
    assert_eq!(preparation.retained_model.unwrap(), model("first"));
    assert_eq!(preparation.retained_events.len(), 3);
    assert!(matches!(
        preparation.retained_events[1].kind,
        EventType::Model(_)
    ));
    let prompt = preparation.history_prompt.unwrap();
    for excluded in ["secret system", "new system", "artifact", "log"] {
        assert!(!prompt.contains(excluded));
    }
}

#[test]
fn new_context_is_a_hard_boundary_and_previous_summary_updates_iteratively() {
    let transcript = events(vec![
        message(MessageType::Human, "obsolete"),
        summary("obsolete summary"),
        EventType::NewContext,
        summary("current summary"),
        message(MessageType::Human, "new history"),
        message(MessageType::Human, "keep"),
    ]);
    let preparation = prepare(&transcript, 1).unwrap();
    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("current summary")
    );
    let prompt = preparation.history_prompt.unwrap();
    assert!(prompt.contains("<previous-summary>\ncurrent summary\n</previous-summary>"));
    assert!(prompt.contains("Update the existing structured summary"));
    assert!(!prompt.contains("obsolete"));
}

#[test]
fn file_tracking_is_sorted_cumulative_and_removes_modified_files_from_reads() {
    let old = "old summary\n\n<read-files>\nold\nboth\n</read-files>\n\n<modified-files>\nprior-write\n</modified-files>";
    let preparation = prepare(
        &events(vec![
            summary(old),
            message(MessageType::Human, "request"),
            start(1),
            call("a", "read", r#"{"path":"z-read"}"#),
            call("b", "write", r#"{"path":"both","content":"text"}"#),
            call("c", "edit", r#"{"path":"a-edit"}"#),
            call("d", "read", "{malformed"),
            call("e", "read", r#"{"path":7}"#),
            call("f", "unknown", r#"{"path":"ignore"}"#),
            end(1, 0),
            result("a", "ok"),
            result("b", "ok"),
            result("c", "ok"),
            result("d", "error"),
            result("e", "error"),
            result("f", "ok"),
            message(MessageType::Human, "keep"),
        ]),
        1,
    )
    .unwrap();
    assert_eq!(preparation.read_files, ["old", "z-read"]);
    assert_eq!(
        preparation.modified_files,
        ["a-edit", "both", "prior-write"]
    );
    assert_eq!(
        preparation.finish_summary(Some("new"), None),
        "new\n\n<read-files>\nold\nz-read\n</read-files>\n\n<modified-files>\na-edit\nboth\nprior-write\n</modified-files>"
    );
    // Prefix operations count too, but retained operations are not prematurely included.
    let split = prepare(
        &events(vec![
            message(MessageType::Human, "request"),
            start(1),
            call("a", "edit", r#"{"path":"prefix"}"#),
            end(1, 0),
            result("a", "ok"),
            start(2),
            call("b", "read", r#"{"path":"retained"}"#),
            end(2, 0),
            result("b", "ok"),
        ]),
        1,
    )
    .unwrap();
    assert_eq!(split.modified_files, ["prefix"]);
    assert!(split.read_files.is_empty());
}

#[test]
fn native_compaction_is_not_silently_replaced_by_a_text_summary() {
    let native = llm_api::api::HistoryItem {
        kind: llm_api::api::HistoryItemKind::ProviderItem(llm_api::api::ProviderItem {
            provider: ProviderType::Openrouter,
            item_type: "compaction".into(),
            raw: serde_json::json!({}),
        }),
        metadata: Default::default(),
    };
    let transcript = events(vec![
        EventType::Compaction(Compaction {
            summary: CompactionSummary::ProviderItem(native),
            usage: Usage::default(),
        }),
        message(MessageType::Human, "old"),
        message(MessageType::Human, "new"),
    ]);
    assert!(prepare(&transcript, 1).is_none());
}

#[test]
fn subagent_attribution_is_preserved_in_the_transcript_and_estimate() {
    let transcript = events(vec![message(MessageType::Subagent, "report")]);
    assert_eq!(
        serialize_conversation(&transcript),
        "[User]: Message from another agent:\nreport"
    );
    assert_eq!(
        estimate_context_tokens(&transcript),
        char_count("Message from another agent:\nreport").div_ceil(4)
    );
}

#[test]
fn serialization_matches_pi_and_omits_thinking_and_images() {
    let mut user = Message::text(MessageType::Human, "a");
    user.content.extend(crate::prompts::text("b"));
    user.content.push(ContentBlock::Image(ImageBlock {
        source: MediaSource::Url("image".into()),
        detail: None,
    }));
    let transcript = events(vec![
        EventType::Message(user),
        start(1),
        thinking("hidden thoughts"),
        message(MessageType::Agent, "first"),
        message(MessageType::Agent, "second"),
        call("a", "read", r#"{"path":"a"}"#),
        call("b", "noop", "{}"),
        end(1, 0),
        result("a", "output"),
        result("b", ""),
        message(MessageType::System, "system"),
    ]);
    assert_eq!(
        serialize_conversation(&transcript),
        "[User]: ab\n\n[Assistant]: first\nsecond\n\n[Assistant tool calls]: read(path=\"a\"); noop()\n\n[Tool result]: output"
    );
}

#[test]
fn result_truncation_uses_two_thousand_characters_not_bytes() {
    let text = "é".repeat(2001);
    let truncated = truncate_result(&text);
    assert_eq!(
        truncated,
        format!("{}\n\n[... 1 more characters truncated]", "é".repeat(2000))
    );
    assert_eq!(truncate_result(&"a".repeat(2000)), "a".repeat(2000));
    let transcript = serialize_conversation(&events(vec![result("a", &"a".repeat(2500))]));
    assert!(transcript.ends_with("[... 500 more characters truncated]"));
    assert_eq!(char_count("😀"), 2);
    assert!(
        truncate_result(&format!("{}😀", "a".repeat(1999)))
            .ends_with("[... 2 more characters truncated]")
    );
}

#[test]
fn estimator_uses_latest_valid_usage_and_only_trailing_estimates() {
    let mut kinds = vec![
        message(MessageType::Human, &"x".repeat(4000)),
        start(1),
        message(MessageType::Agent, "answer"),
        end(1, 100),
        result("a", "12345678"),
        message(MessageType::Human, "12345"),
    ];
    assert_eq!(estimate_context_tokens(&events(kinds.clone())), 104);
    for outcome in [
        Outcome::Cancelled,
        Outcome::Failed(llm_api::Error::InvalidRequest {
            message: "bad".into(),
        }),
        Outcome::InProgress,
    ] {
        let mut ending = match end(2, 9000) {
            EventType::InferenceResultEnd(end) => end,
            _ => unreachable!(),
        };
        ending.outcome = outcome;
        let mut failed = kinds.clone();
        failed.extend([
            start(2),
            message(MessageType::Agent, "1234"),
            EventType::InferenceResultEnd(ending),
        ]);
        assert_eq!(estimate_context_tokens(&events(failed)), 105);
    }
    kinds.extend([start(2), message(MessageType::Agent, "1234"), end(2, 0)]);
    assert_eq!(estimate_context_tokens(&events(kinds.clone())), 105);
    kinds.extend([start(3), message(MessageType::Agent, "1234"), end(3, 200)]);
    assert_eq!(estimate_context_tokens(&events(kinds)), 200);
}

#[test]
fn usage_fallback_does_not_double_count_cached_or_reasoning_subsets() {
    let usage = Usage {
        total_tokens: Some(0),
        input_tokens: Some(50),
        output_tokens: Some(10),
        cached_input_tokens: Some(40),
        reasoning_output_tokens: Some(8),
        ..Default::default()
    };
    assert_eq!(usage_tokens(&usage), 60);
    assert_eq!(
        usage_tokens(&Usage {
            total_tokens: Some(70),
            ..usage
        }),
        70
    );
    assert_eq!(
        usage_tokens(&Usage {
            input_tokens: Some(u64::MAX),
            output_tokens: Some(1),
            ..Default::default()
        }),
        u64::MAX
    );
}

#[test]
fn length_limited_responses_still_supply_usage() {
    let mut ending = match end(1, 100) {
        EventType::InferenceResultEnd(end) => end,
        _ => unreachable!(),
    };
    ending.outcome = Outcome::Incomplete {
        reason: StopReason::TokenLimit,
    };
    assert_eq!(
        estimate_context_tokens(&events(vec![
            start(1),
            EventType::InferenceResultEnd(ending)
        ])),
        100
    );
}

#[test]
fn estimator_resets_at_new_context_and_compaction_invalidates_old_usage() {
    let kinds = vec![
        start(1),
        message(MessageType::Agent, "1234"),
        end(1, 999),
        EventType::NewContext,
        summary("12345678"),
        message(MessageType::Agent, "1234"),
    ];
    assert_eq!(estimate_context_tokens(&events(kinds)), 3);
    let kinds = vec![
        start(1),
        message(MessageType::Agent, "1234"),
        end(1, 999),
        summary("12345678"),
    ];
    assert_eq!(estimate_context_tokens(&events(kinds)), 3);
}

#[test]
fn tool_argument_estimates_use_compact_json_and_keep_malformed_input() {
    assert_eq!(
        event_chars(&call("a", "read", "{ \"path\" : \"a\" }")),
        char_count("read") + char_count(r#"{"path":"a"}"#)
    );
    assert_eq!(
        event_chars(&call("a", "read", "{malformed")),
        char_count("read{malformed")
    );
}

#[test]
fn fallback_estimates_systems_tools_images_and_thinking() {
    let mut user = Message::text(MessageType::Human, "1234");
    user.content.push(ContentBlock::Image(ImageBlock {
        source: MediaSource::Url("image".into()),
        detail: None,
    }));
    let tool = ToolDefinition {
        id: ToolId::Read,
        definition: crate::components::tools::tool_definition(ToolId::Read).unwrap(),
    };
    let overhead = char_count(&serde_json::to_string(&tool.definition).unwrap()).div_ceil(4);
    let kinds = vec![
        message(MessageType::System, "12345"),
        EventType::ToolDefinition(tool),
        EventType::Message(user),
        start(1),
        thinking("12345"),
        message(MessageType::Agent, "123"),
        end(1, 0),
    ];
    assert_eq!(
        estimate_context_tokens(&events(kinds)),
        2 + overhead + 1201 + 2
    );
}

#[test]
fn retained_context_replays_without_old_usage_or_result_markers() {
    let original = events(vec![
        EventType::Model(model("test")),
        message(MessageType::Human, "request"),
        start(1),
        thinking("reasoning"),
        call("a", "read", r#"{"path":"a"}"#),
        end(1, 999),
        result("a", "output"),
    ]);
    let preparation = prepare(&original, 1).unwrap();
    let mut kinds = vec![
        EventType::NewContext,
        EventType::Model(preparation.retained_model.clone().unwrap()),
        summary("checkpoint"),
    ];
    kinds.extend(
        preparation
            .retained_events
            .into_iter()
            .map(|event| event.kind),
    );
    let compacted = events(kinds);
    let projection = crate::agents::agent_projection::AgentProjection::replay(&compacted).unwrap();
    assert!(projection.last_response().is_none());
    assert!(projection.prepare_request(RequestConfig::default()).is_ok());
    assert!(estimate_context_tokens(&compacted) < 999);
}
