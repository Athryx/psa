use super::*;
use crate::agents::AgentId;
use llm_api::{
    api::{Outcome, ProviderOpaqueData, Reasoning, StopReason, Usage},
    providers::{Effort, Model, ProviderType},
};
use std::time::Duration;

fn config() -> ModelConfig {
    ModelConfig {
        model: Model::new("test").unwrap(),
        provider: ProviderType::Openrouter,
        effort: Effort::High,
    }
}
fn events(kinds: Vec<EventType>) -> Vec<Event> {
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Event {
            id: EventId(i as u64 + 1),
            source_agent_id: AgentId(1),
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            kind,
        })
        .collect()
}
fn start(id: u64) -> EventType {
    EventType::InferenceResultStart(InferenceResultStart {
        id,
        model_config: config(),
        request_config: RequestConfig::default(),
    })
}
fn end(id: u64, reason: StopReason) -> EventType {
    EventType::InferenceResultEnd(InferenceResultEnd {
        id,
        outcome: Outcome::Completed { reason },
        usage: Usage {
            output_tokens: Some(4),
            ..Default::default()
        },
        request_time: Duration::from_millis(10),
        response_id: Some(format!("response-{id}")),
        provider_metadata: Default::default(),
    })
}
fn message(kind: MessageType, contents: &str) -> EventType {
    EventType::Message(Message::text(kind, contents))
}
fn thinking() -> EventType {
    EventType::Thinking(Thinking {
        contents: Reasoning {
            summary: vec!["reason".into()],
            opaque_state: Some(ProviderOpaqueData {
                provider: ProviderType::Openrouter,
                data: serde_json::json!("encrypted"),
            }),
        },
        metadata: HistoryItemMetadata::default(),
    })
}

#[test]
fn renders_flat_context_and_tracks_the_latest_response_separately() {
    let transcript = events(vec![
        EventType::Model(config()),
        EventType::ToolDefinition(ToolDefinition::for_tool(ToolId::Noop)),
        message(MessageType::Human, "question"),
        start(1),
        thinking(),
        EventType::ToolCall(ToolCall {
            call_id: "call-1".into(),
            tool_name: "noop".into(),
            arguments: "{bad-json".into(),
            metadata: HistoryItemMetadata::default(),
        }),
        end(1, StopReason::ToolCalls),
        EventType::ToolResult(ToolResult {
            call_id: "call-1".into(),
            content: vec![build_text_block("invalid arguments")],
            error: true,
            metadata: Default::default(),
        }),
        start(2),
        message(MessageType::Agent, "answer"),
        end(2, StopReason::EndTurn),
        EventType::Log(Log {
            error: false,
            message: "not model input".into(),
        }),
        EventType::StatusChange(StatusChange {
            new_status: AgentState::Idle,
            reason: None,
        }),
    ]);
    let unresolved = AgentContext::replay(&transcript[..7]).unwrap();
    assert!(matches!(
        unresolved.prepare_request(RequestConfig::default()),
        Err(RequestError::Invalid(_))
    ));
    let state = AgentContext::replay(&transcript).unwrap();
    let prepared = state.prepare_request(RequestConfig::default()).unwrap();
    assert_eq!(prepared.history.items.len(), 5);
    let response = state.last_response().unwrap();
    assert_eq!(response.items.len(), 1);
    assert_eq!(response.config, config());
    assert_eq!(response.usage.output_tokens, Some(4));
    assert_eq!(response.response_id.as_deref(), Some("response-2"));
    assert!(
        matches!(&prepared.history.items[3].kind,HistoryItemKind::ToolResult(result) if result.is_error)
    );
    let mut incremental = AgentContext::default();
    for event in &transcript {
        incremental.apply_event(event).unwrap();
    }
    assert_eq!(format!("{:?}", state), format!("{:?}", incremental));
}

#[test]
fn compaction_and_retained_messages_go_directly_into_history() {
    let transcript = events(vec![
        EventType::Model(config()),
        message(MessageType::Human, "old"),
        start(1),
        message(MessageType::Agent, "old response"),
        end(1, StopReason::EndTurn),
        EventType::StatusChange(StatusChange {
            new_status: AgentState::Interrupted,
            reason: Some("user".into()),
        }),
        EventType::NewContext,
        EventType::Model(config()),
        EventType::Compaction(Compaction {
            summary: CompactionSummary::Text("summary".into()),
            usage: Usage::default(),
        }),
        message(MessageType::Agent, "retained"),
        message(MessageType::Human, "next"),
        start(2),
        message(MessageType::Agent, "new response"),
        end(2, StopReason::EndTurn),
    ]);
    assert!(matches!(
        AgentContext::replay(&transcript[..7])
            .unwrap()
            .prepare_request(RequestConfig::default()),
        Err(RequestError::MissingModel)
    ));
    let context = AgentContext::replay(&transcript[..11]).unwrap();
    let request = context.prepare_request(RequestConfig::default()).unwrap();
    assert_eq!(request.history.items.len(), 3);
    assert!(
        matches!(&request.history.items[1].kind,HistoryItemKind::Message(m) if m.role == Role::Assistant)
    );
    assert_eq!(context.status, AgentState::Interrupted);
    let finished = AgentContext::replay(&transcript)
        .unwrap()
        .prepare_request(RequestConfig::default())
        .unwrap();
    assert_eq!(finished.history.items.len(), 4);
}

#[test]
fn rejects_unfinished_or_mismatched_result_blocks_without_mutating_state() {
    let transcript = events(vec![
        EventType::Model(config()),
        start(1),
        end(2, StopReason::EndTurn),
    ]);
    let mut state = AgentContext::replay(&transcript[..2]).unwrap();
    assert!(matches!(
        state.prepare_request(RequestConfig::default()),
        Err(RequestError::OpenInferenceResult(1))
    ));
    let before = format!("{state:?}");
    assert!(state.apply_event(&transcript[2]).is_err());
    assert_eq!(before, format!("{state:?}"));
    let mut duplicate = transcript[1].clone();
    duplicate.id = EventId(3);
    assert!(state.apply_event(&duplicate).is_err());
    assert!(matches!(
        state.apply_event(&transcript[0]),
        Err(ProjectionError::EventSequence { .. })
    ));
}

#[test]
fn model_switches_convert_context_but_preserve_the_response_snapshot() {
    let mut new = config();
    new.provider = ProviderType::OpenAiCodex;
    new.model = Model::new("other").unwrap();
    let transcript = events(vec![
        EventType::Model(config()),
        message(MessageType::Human, "question"),
        start(1),
        thinking(),
        message(MessageType::Agent, "answer"),
        end(1, StopReason::EndTurn),
        thinking(),
        message(MessageType::Agent, "retained"),
        EventType::Model(new.clone()),
    ]);
    let state = AgentContext::replay(&transcript).unwrap();
    let request = state.prepare_request(RequestConfig::default()).unwrap();
    assert_eq!(request.history.config, new);
    assert_eq!(state.last_response().unwrap().config, config());
    assert_eq!(state.last_response().unwrap().items.len(), 2);
    assert_eq!(request.history.items.len(), 3);
}

#[test]
fn preserves_rich_content_and_checks_snapshotted_tools() {
    let rich = Message {
        message_type: MessageType::Agent,
        delivery: None,
        content: vec![
            api::ContentBlock::Text(api::TextBlock {
                text: "actual text".into(),
                annotations: vec![api::Annotation {
                    provider: ProviderType::Openrouter,
                    annotation_type: "citation".into(),
                    raw: serde_json::json!({"url":"https://example.test"}),
                }],
            }),
            api::ContentBlock::Image(api::ImageBlock {
                source: api::MediaSource::Url("https://example.test/image.png".into()),
                detail: None,
            }),
        ],
        metadata: HistoryItemMetadata {
            item_id: Some("message-1".into()),
            ..Default::default()
        },
    };
    let mut tool = ToolDefinition::for_tool(ToolId::Noop);
    tool.definition.description = "stored definition".into();
    let transcript = events(vec![
        EventType::Model(config()),
        EventType::ToolDefinition(tool.clone()),
        EventType::Message(rich),
        EventType::ToolDefinition(tool),
    ]);
    let mut state = AgentContext::replay(&transcript[..3]).unwrap();
    let request = state.prepare_request(RequestConfig::default()).unwrap();
    assert_eq!(request.history.tools[0].description, "stored definition");
    let HistoryItemKind::Message(message) = &request.history.items[0].kind else {
        panic!()
    };
    let api::ContentBlock::Text(text) = &message.content[0] else {
        panic!()
    };
    assert_eq!(text.annotations.len(), 1);
    assert_eq!(text.text, "actual text");
    assert_eq!(message.content.len(), 2);
    assert!(matches!(&message.content[1], api::ContentBlock::Image(_)));
    assert_eq!(
        request.history.items[0].metadata.item_id.as_deref(),
        Some("message-1")
    );
    assert!(state.apply_event(&transcript[3]).is_err());
}

#[test]
fn renders_plain_and_empty_message_content() {
    let transcript = events(vec![
        message(MessageType::Human, "hello"),
        EventType::Message(Message {
            message_type: MessageType::Agent,
            delivery: None,
            content: vec![],
            metadata: Default::default(),
        }),
    ]);

    let context = AgentContext::replay(&transcript).unwrap();
    let HistoryItemKind::Message(plain) = &context.items()[0].kind else {
        panic!()
    };
    assert_eq!(plain.role, Role::User);
    assert_eq!(plain.content.len(), 1);
    assert!(
        matches!(&plain.content[0], api::ContentBlock::Text(text) if text.text == "hello" && text.annotations.is_empty())
    );
    let HistoryItemKind::Message(empty) = &context.items()[1].kind else {
        panic!()
    };
    assert_eq!(empty.role, Role::Assistant);
    assert!(empty.content.is_empty());
}

#[test]
fn preserves_tool_result_content_blocks() {
    let content: crate::tools::ToolResult = vec![
        build_text_block("tool output"),
        api::ContentBlock::Image(api::ImageBlock {
            source: api::MediaSource::Url("https://example.test/output.png".into()),
            detail: None,
        }),
    ];
    let metadata = HistoryItemMetadata {
        item_id: Some("result-1".into()),
        ..Default::default()
    };
    let transcript = events(vec![
        EventType::ToolResult(ToolResult {
            call_id: "call-1".into(),
            content: content.clone(),
            error: true,
            metadata: metadata.clone(),
        }),
        EventType::ToolResult(ToolResult {
            call_id: "call-2".into(),
            content: vec![],
            error: false,
            metadata: Default::default(),
        }),
    ]);

    let state = AgentContext::replay(&transcript).unwrap();
    let HistoryItemKind::ToolResult(result) = &state.items()[0].kind else {
        panic!()
    };
    assert_eq!(result.call_id, "call-1");
    assert!(result.is_error);
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::to_value(content).unwrap()
    );
    assert_eq!(state.items()[0].metadata.item_id, metadata.item_id);
    let HistoryItemKind::ToolResult(empty) = &state.items()[1].kind else {
        panic!()
    };
    assert!(empty.content.is_empty());
    assert!(!empty.is_error);
}

#[test]
fn result_blocks_reject_prompt_items_and_mismatched_model_snapshots() {
    let transcript = events(vec![EventType::Model(config()), start(1)]);
    let mut state = AgentContext::replay(&transcript).unwrap();
    let before = format!("{state:?}");

    for kind in [
        message(MessageType::Human, "too late"),
        message(MessageType::Harness, "injected"),
        message(MessageType::System, "instructions"),
        EventType::ToolResult(ToolResult {
            call_id: "call".into(),
            content: vec![build_text_block("result")],
            error: false,
            metadata: Default::default(),
        }),
    ] {
        let event = Event {
            id: EventId(3),
            kind,
            ..transcript[0].clone()
        };
        assert!(state.apply_event(&event).is_err());
        assert_eq!(format!("{state:?}"), before);
    }

    let mut state = AgentContext::replay(&transcript[..1]).unwrap();
    let mut event = transcript[1].clone();
    let EventType::InferenceResultStart(start) = &mut event.kind else {
        panic!()
    };
    start.model_config.model = Model::new("different-model").unwrap();
    let before = format!("{state:?}");
    assert!(state.apply_event(&event).is_err());
    assert_eq!(format!("{state:?}"), before);
}

#[test]
fn empty_failed_and_cancelled_results_are_complete_blocks() {
    for outcome in [
        Outcome::Completed {
            reason: StopReason::EndTurn,
        },
        Outcome::Incomplete {
            reason: StopReason::TokenLimit,
        },
        Outcome::Failed(llm_api::Error::Transport {
            message: "offline".into(),
        }),
        Outcome::Cancelled,
    ] {
        let mut finish = end(1, StopReason::EndTurn);
        let EventType::InferenceResultEnd(result) = &mut finish else {
            panic!()
        };
        result.outcome = outcome.clone();
        let transcript = events(vec![
            EventType::Model(config()),
            message(MessageType::Human, "question"),
            start(1),
            finish,
        ]);
        let state = AgentContext::replay(&transcript).unwrap();
        assert!(state.open_inference_result().is_none());
        let response = state.last_response().unwrap();
        assert_eq!(format!("{:?}", response.outcome), format!("{outcome:?}"));
        assert!(response.items.is_empty());
        let request = state.prepare_request(RequestConfig::default()).unwrap();
        assert_eq!(request.history.items.len(), 1);
    }
}
