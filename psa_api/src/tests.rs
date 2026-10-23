use super::*;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};

fn model_config() -> ModelConfig {
    ModelConfig {
        model: Model::new("test-model").unwrap(),
        provider: ProviderType::Openrouter,
        effort: Effort::High,
    }
}

#[test]
fn definitions_have_flat_versioned_wire_types_and_default_empty_hooks() {
    let request: CreateAgentDefinition = serde_json::from_value(serde_json::json!({
        "name": "assistant",
    }))
    .unwrap();
    assert_eq!(request.name, "assistant");
    assert_eq!(request.hooks, HookIds::default());
    assert!(request.tools.is_empty());
    assert!(request.bundles.is_empty());
    assert!(request.model_config.is_none());
    assert!(request.skills.is_empty());
    assert!(request.permitted_subagents.is_empty());
    assert!(request.extra_system_prompt.is_empty());
    assert_eq!(
        serde_json::from_str::<HookIds>("{}").unwrap(),
        HookIds::default()
    );
    let hooks = HookIds {
        agent_create: vec![
            HookId::DefaultStartup,
            HookId::ExtraSystemPrompt,
            HookId::ProjectContext,
        ],
        session_update: vec![HookId::SessionModelUpdate],
        before_request: vec![HookId::Noop],
        after_request: vec![HookId::StopWithoutTools],
    };
    let definition = AgentDefinition {
        id: AgentDefinitionId(9),
        previous_version_id: Some(AgentDefinitionId(4)),
        version: 2,
        name: request.name,
        description: "Reusable assistant".into(),
        selectable: true,
        bundles: vec![BundleId::FileTools],
        tools: vec![ToolId::Noop],
        hooks: hooks.clone(),
        model_config: Some(model_config()),
        skills: vec!["/skills/example".into()],
        permitted_subagents: vec![AgentDefinitionId(3)],
        extra_system_prompt: "Extra instructions".into(),
    };
    let json = serde_json::to_value(&definition).unwrap();
    assert_eq!(json["id"], 9);
    assert_eq!(json["previous_version_id"], 4);
    assert_eq!(json["version"], 2);
    assert_eq!(json["bundles"], serde_json::json!(["FileTools"]));
    assert_eq!(json["hooks"]["agent_create"][0], "DefaultStartup");
    assert_eq!(json["hooks"]["agent_create"][1], "ExtraSystemPrompt");
    assert_eq!(json["hooks"]["agent_create"][2], "ProjectContext");
    assert_eq!(json["skills"][0], "/skills/example");
    assert_eq!(json["permitted_subagents"][0], 3);
    assert!(json.get("config").is_none());
    let decoded: AgentDefinition = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded.bundles, vec![BundleId::FileTools]);
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("bundles");
    let legacy: AgentDefinition = serde_json::from_value(legacy).unwrap();
    assert!(legacy.bundles.is_empty());
    assert_eq!(decoded.hooks, hooks);
    assert_eq!(decoded.model_config, Some(model_config()));
    assert!(
        serde_json::from_value::<CreateAgentDefinition>(serde_json::json!({
            "name": "assistant", "extras": {}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<UpdateAgentDefinition>(serde_json::json!({
            "description": "not a complete replacement"
        }))
        .is_err()
    );
}

#[test]
fn bundle_requests_roundtrip_and_replacement_defaults_clear_bundles() {
    let request = CreateAgentDefinition {
        name: "files".into(),
        bundles: vec![BundleId::FileTools],
        ..Default::default()
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["bundles"], serde_json::json!(["FileTools"]));
    let decoded: CreateAgentDefinition = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded.bundles, request.bundles);
    let decoded: UpdateAgentDefinition = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.bundles, request.bundles);
    let replacement: UpdateAgentDefinition =
        serde_json::from_value(serde_json::json!({"name": "replacement"})).unwrap();
    assert!(replacement.bundles.is_empty());
    assert!(CreateAgentDefinition::default().bundles.is_empty());
    for bundles in [serde_json::json!(["Unknown"]), serde_json::Value::Null] {
        assert!(
            serde_json::from_value::<CreateAgentDefinition>(serde_json::json!({
                "name": "invalid", "bundles": bundles
            }))
            .is_err()
        );
    }
}

#[test]
fn session_models_are_required_and_agents_reference_definitions() {
    let mut session = serde_json::json!({
        "name": "session",
        "working_directory": "/work",
        "probe_id": "00000000-0000-0000-0000-000000000000"
    });
    assert!(serde_json::from_value::<CreateSession>(session.clone()).is_err());
    session["model_config"] = serde_json::to_value(model_config()).unwrap();
    let decoded: CreateSession = serde_json::from_value(session.clone()).unwrap();
    assert_eq!(decoded.model_config, model_config());
    session["secret"] = serde_json::json!("not allowed");
    assert!(serde_json::from_value::<CreateSession>(session).is_err());
    let update: UpdateSession = serde_json::from_str("{}").unwrap();
    assert!(update.model_config.is_none());
    let update: UpdateSession = serde_json::from_value(serde_json::json!({
        "model_config": model_config()
    }))
    .unwrap();
    assert_eq!(update.model_config, Some(model_config()));
    let agent = CreateAgent {
        interactive: false,
        definition_id: AgentDefinitionId(7),
        name: "agent".into(),
    };
    let mut json = serde_json::to_value(agent).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"definition_id": 7, "name": "agent", "interactive": false})
    );
    assert!(serde_json::from_value::<CreateAgent>(json.clone()).is_ok());
    json["model"] = serde_json::to_value(model_config()).unwrap();
    assert!(serde_json::from_value::<CreateAgent>(json).is_err());
}

#[test]
fn agent_interactive_wire_field_defaults_false_and_round_trips_bools() {
    let omitted: CreateAgent = serde_json::from_value(serde_json::json!({
        "definition_id": 7,
        "name": "agent"
    }))
    .unwrap();
    assert!(!omitted.interactive);

    for interactive in [false, true] {
        let request = CreateAgent {
            definition_id: AgentDefinitionId(7),
            name: "agent".into(),
            interactive,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["interactive"], interactive);
        let decoded: CreateAgent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.interactive, interactive);

        let agent = Agent {
            id: AgentId(1),
            session_id: SessionId(2),
            definition_id: request.definition_id,
            parent_id: None,
            name: request.name,
            interactive,
            state: AgentState::Idle,
            usage: AgentUsage::default(),
            next_event_id: events::EventId(1),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let json = serde_json::to_value(&agent).unwrap();
        assert_eq!(json["interactive"], interactive);
        let decoded: Agent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.interactive, interactive);
    }

    for invalid in [
        serde_json::Value::Null,
        serde_json::json!("true"),
        serde_json::json!(1),
    ] {
        assert!(
            serde_json::from_value::<CreateAgent>(serde_json::json!({
                "definition_id": 7, "name": "agent", "interactive": invalid
            }))
            .is_err()
        );
    }
}

#[test]
fn wire_ids_timestamps_and_message_requests_have_one_representation() {
    let event = events::Event {
        id: events::EventId(3),
        source_agent_id: AgentId(7),
        timestamp: time::OffsetDateTime::UNIX_EPOCH,
        kind: events::EventType::NewContext,
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["id"], 3);
    assert_eq!(json["source_agent_id"], 7);
    assert_eq!(json["timestamp"], "1970-01-01T00:00:00Z");
    let decoded: events::Event = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.id, event.id);
    let request = AgentRequest::Message {
        delivery: MessageDelivery::Steer,
        content: vec![],
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(!json.contains("replay_content"));
    assert!(serde_json::from_str::<AgentRequest>(&json).is_ok());
    assert!(
        serde_json::from_str::<CreateSession>(
            r#"{"name":"test","working_directory":"/work","secret":"not allowed"}"#
        )
        .is_err()
    );
}

#[test]
fn client_query_wire_types_roundtrip_and_default_optional_fields() {
    let id = ClientQueryId::new();
    assert_eq!(id.0.get_version_num(), 4);
    assert_eq!(serde_json::to_value(id).unwrap(), id.to_string());
    assert_eq!(
        serde_json::from_value::<ClientQueryId>(serde_json::json!(id.to_string())).unwrap(),
        id
    );
    let payload = serde_json::json!({
        "type": "ask_question",
        "payload": {"questions": [{"question": "Choose a direction"}]}
    });
    let query: ClientQuery = serde_json::from_value(payload.clone()).unwrap();
    let ClientQuery::AskQuestion(request) = &query;
    assert!(request.questions[0].choices.is_empty());
    assert!(query.validate().is_ok());
    let request = ClientQueryRequest {
        id,
        session_id: SessionId(1),
        agent_id: AgentId(2),
        payload: query,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["payload"]["type"], "ask_question");
    assert_eq!(
        serde_json::from_value::<ClientQueryRequest>(json).unwrap(),
        request
    );

    let response: ClientQueryResponse = serde_json::from_value(serde_json::json!({
        "type": "ask_question",
        "payload": {"answers": [{"answer": "Another direction"}]}
    }))
    .unwrap();
    let ClientQueryResponse::AskQuestion(answers) = &response;
    assert!(answers.answers[0].notes.is_none());
    assert!(request.payload.validate_response(&response).is_ok());
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["payload"]["answers"][0]["answer"], "Another direction");
    assert_eq!(
        serde_json::from_value::<ClientQueryResponse>(json).unwrap(),
        response
    );

    let requested = StreamMessage::ClientQueryRequested {
        query: request.clone(),
    };
    let json = serde_json::to_value(&requested).unwrap();
    assert_eq!(json["type"], "client_query_requested");
    assert!(
        matches!(serde_json::from_value::<StreamMessage>(json).unwrap(), StreamMessage::ClientQueryRequested { query } if query == request)
    );
    let removed = StreamMessage::ClientQueryRemoved {
        session_id: request.session_id,
        agent_id: request.agent_id,
        query_id: id,
    };
    let json = serde_json::to_value(removed).unwrap();
    assert_eq!(json["type"], "client_query_removed");
    assert!(
        matches!(serde_json::from_value::<StreamMessage>(json).unwrap(), StreamMessage::ClientQueryRemoved { query_id, .. } if query_id == id)
    );
}

#[test]
fn client_query_validation_requires_nonblank_input_and_exact_answer_count() {
    let query = |questions| ClientQuery::AskQuestion(AskQuestionRequest { questions });
    assert!(query(vec![]).validate().is_err());
    assert!(
        query(vec![ClientQuestion {
            question: " \n\t".into(),
            choices: vec![]
        }])
        .validate()
        .is_err()
    );
    assert!(
        query(vec![ClientQuestion {
            question: "Question".into(),
            choices: vec![" \n".into()]
        }])
        .validate()
        .is_err()
    );
    let query = query(vec![
        ClientQuestion {
            question: "First?".into(),
            choices: vec!["Yes".into(), "No".into()],
        },
        ClientQuestion {
            question: "Second?".into(),
            choices: vec![],
        },
    ]);
    assert!(query.validate().is_ok());
    let response = |answers: Vec<&str>| {
        ClientQueryResponse::AskQuestion(AskQuestionResponse {
            answers: answers
                .into_iter()
                .map(|answer| ClientAnswer {
                    answer: answer.into(),
                    notes: None,
                })
                .collect(),
        })
    };
    for answers in [
        vec![],
        vec!["Yes"],
        vec!["Yes", "No", "Extra"],
        vec!["Yes", " \n\t"],
    ] {
        assert!(query.validate_response(&response(answers)).is_err());
    }
    // Choices are suggestions: an Other/freeform response is valid.
    assert!(
        query
            .validate_response(&response(vec!["An unlisted choice", "Freeform answer"]))
            .is_ok()
    );
    let response = ClientQueryResponse::AskQuestion(AskQuestionResponse {
        answers: vec![
            ClientAnswer {
                answer: "Yes".into(),
                notes: Some("Optional detail".into()),
            },
            ClientAnswer {
                answer: "Second answer".into(),
                notes: Some(String::new()),
            },
        ],
    });
    assert!(query.validate_response(&response).is_ok());
}

#[test]
fn client_query_wire_types_reject_unknown_fields_and_variants() {
    let request = serde_json::json!({
        "type": "ask_question", "payload": {"questions": [{"question": "Question"}]}
    });
    for invalid in [
        serde_json::json!({"type": "unknown", "payload": {}}),
        serde_json::json!({"type": "ask_question", "payload": {"questions": []}, "extra": true}),
        serde_json::json!({"type": "ask_question", "payload": {"questions": [], "extra": true}}),
        serde_json::json!({"type": "ask_question", "payload": {"questions": [{"question": "Q", "extra": true}]}}),
    ] {
        assert!(serde_json::from_value::<ClientQuery>(invalid).is_err());
    }
    for invalid in [
        serde_json::json!({"type": "unknown", "payload": {}}),
        serde_json::json!({"type": "ask_question", "payload": {"answers": []}, "extra": true}),
        serde_json::json!({"type": "ask_question", "payload": {"answers": [], "extra": true}}),
        serde_json::json!({"type": "ask_question", "payload": {"answers": [{"answer": "A", "extra": true}]}}),
    ] {
        assert!(serde_json::from_value::<ClientQueryResponse>(invalid).is_err());
    }
    assert!(serde_json::from_value::<ClientQueryRequest>(serde_json::json!({
        "id": ClientQueryId::new(), "session_id": 1, "agent_id": 2, "payload": request, "extra": true,
    })).is_err());
}

#[test]
fn legacy_event_snapshots_and_pages_default_queries_to_empty() {
    let snapshot: StreamMessage = serde_json::from_value(serde_json::json!({
        "type": "snapshot", "cursor": 5, "pending": []
    }))
    .unwrap();
    assert!(matches!(snapshot, StreamMessage::Snapshot { queries, .. } if queries.is_empty()));
    let page: EventsPage = serde_json::from_value(serde_json::json!({
        "events": [], "pending": [], "next_after": 5, "head": 5, "has_more": false
    }))
    .unwrap();
    assert!(page.queries.is_empty());
    assert_eq!(
        serde_json::to_value(page).unwrap()["queries"],
        serde_json::json!([])
    );
}

#[test]
fn paths_and_client_base_urls_are_validated() {
    assert_eq!(
        routes::agent_path(routes::AGENT_EVENTS, SessionId(1), AgentId(2)),
        "/sessions/1/agents/2/events"
    );
    assert_eq!(
        routes::agent_definition_path(AgentDefinitionId(42)),
        "/agent-definitions/42"
    );
    let query = ClientQueryId(uuid::Uuid::nil());
    assert_eq!(
        routes::client_query_response_path(SessionId(1), AgentId(2), query),
        "/sessions/1/agents/2/queries/00000000-0000-0000-0000-000000000000/response"
    );
    for invalid in [
        "invalid",
        "file:///tmp",
        "https://example.com/?secret=x",
        "https://example.com/#x",
    ] {
        assert!(Client::new(invalid).is_err());
    }
    assert!(Client::new("http://localhost:8080/prefix/").is_ok());
}
