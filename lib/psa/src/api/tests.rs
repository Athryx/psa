use super::*;
use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};

fn model_config() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::High,
    }
}

#[test]
fn model_identity_is_shared_by_configuration_and_pin_requests() {
    let model = Model::new(ProviderType::Openrouter, "vendor/model").unwrap();
    let identity = serde_json::json!("openrouter/vendor/model");
    let config = ModelConfig {
        model: model.clone(),
        effort: Effort::High,
    };
    let config_wire = serde_json::json!({"model": identity, "effort": "High"});
    assert_eq!(serde_json::to_value(&config).unwrap(), config_wire);
    assert_eq!(
        serde_json::from_value::<ModelConfig>(config_wire).unwrap(),
        config
    );

    let pin = PinModel {
        model,
        pinned: true,
    };
    let pin_wire = serde_json::json!({"model": identity, "pinned": true});
    assert_eq!(serde_json::to_value(&pin).unwrap(), pin_wire);
    let decoded: PinModel = serde_json::from_value(pin_wire).unwrap();
    assert_eq!(decoded.model, pin.model);
    assert!(decoded.pinned);
}

#[test]
fn config_refresh_lists_sessions_and_rejects_unknown_fields() {
    let request = RefreshConfigs {
        session_ids: vec![SessionId(7)],
    };
    let wire = serde_json::json!({"session_ids": [7]});
    assert_eq!(serde_json::to_value(&request).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<RefreshConfigs>(wire).unwrap(),
        request
    );
    assert!(
        serde_json::from_value::<RefreshConfigs>(serde_json::json!({"refresh_session_id": 7}))
            .is_err()
    );
}

#[test]
fn configured_agent_requests_carry_typed_model_and_effort_selections() {
    for model in [
        None,
        Some(ModelSelection::Global),
        Some(ModelSelection::Parent),
        Some(ModelSelection::Fixed {
            model: Model::parse_slug("openrouter/vendor/model").unwrap(),
            effort: EffortSelection::Fixed(Effort::High),
        }),
    ] {
        let request = CreateAgent {
            config_id: ConfigId {
                config_group: ConfigGroup::Global,
                name: "worker".into(),
            },
            name: "child".into(),
            subagent_parent_id: None,
            interactive: false,
            hidden: false,
            model: model.clone(),
        };
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<CreateAgent>(wire).unwrap().model,
            model
        );
    }

    for model in [
        serde_json::json!("openrouter/vendor/model"),
        serde_json::json!("global"),
        serde_json::json!({"Fixed": {"model": "openrouter/vendor/model"}}),
        serde_json::json!({"Fixed": {"model": "openrouter/vendor/model", "effort": 42}}),
    ] {
        assert!(
            serde_json::from_value::<CreateAgent>(serde_json::json!({
                "config_id": {"config_group": "Global", "name": "worker"},
                "name": "child",
                "model": model,
            }))
            .is_err()
        );
    }
}

#[test]
fn provider_info_only_transmits_identity_and_authentication() {
    let wire = serde_json::json!({"id": "Openrouter", "authed": true});
    let provider: ProviderInfo = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(provider.id, ProviderType::Openrouter);
    assert!(provider.authed);
    assert_eq!(provider.id.name(), "OpenRouter");
    assert_eq!(serde_json::to_value(provider).unwrap(), wire);
}

#[test]
fn definitions_have_flat_versioned_wire_types_and_only_bundles() {
    let bundles = vec![
        BundleConfig::Startup {
            base_prompt: true,
            project_context: true,
            cwd: false,
            skills: true,
        },
        BundleConfig::ExtraSystemPrompt {
            prompt: "Extra instructions".into(),
        },
        BundleConfig::FileTools { read_only: false },
        BundleConfig::StopWithoutTools,
    ];
    let definition = AgentDefinition {
        id: AgentDefinitionId(9),
        previous_version_id: Some(AgentDefinitionId(4)),
        version: 2,
        name: "assistant".into(),
        description: "Reusable assistant".into(),
        selectable: true,
        bundles: bundles.clone(),
        model: model_config().into(),
        skills: vec!["/skills/example".into()],
        permitted_subagents: vec![AgentDefinitionId(3)],
    };
    let json = serde_json::to_value(&definition).unwrap();
    assert_eq!(json["id"], 9);
    assert_eq!(json["previous_version_id"], 4);
    assert_eq!(json["version"], 2);
    assert_eq!(
        json["bundles"],
        serde_json::json!([
            {"Startup": {"base_prompt": true, "project_context": true, "cwd": false, "skills": true}},
            {"ExtraSystemPrompt": {"prompt": "Extra instructions"}},
            {"FileTools": {"read_only": false}},
            "StopWithoutTools",
        ])
    );
    assert!(json.get("tools").is_none());
    assert!(json.get("hooks").is_none());
    assert!(json.get("extra_system_prompt").is_none());
    assert_eq!(json["skills"][0], "/skills/example");
    assert_eq!(json["permitted_subagents"][0], 3);
    assert!(json.get("config").is_none());
    let decoded: AgentDefinition = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(decoded.bundles, bundles);
    assert_eq!(decoded.model, model_config().into());
}

#[test]
fn session_models_are_optional() {
    let mut session = serde_json::json!({
        "name": "session",
        "working_directory": "/work",
        "probe_id": "00000000-0000-0000-0000-000000000000"
    });
    let omitted: CreateSession = serde_json::from_value(session.clone()).unwrap();
    assert_eq!(omitted.model_config, None);
    assert!(
        serde_json::to_value(&omitted)
            .unwrap()
            .get("model_config")
            .is_none()
    );
    session["model_config"] = serde_json::Value::Null;
    assert_eq!(
        serde_json::from_value::<CreateSession>(session.clone())
            .unwrap()
            .model_config,
        None
    );
    session["model_config"] = serde_json::to_value(model_config()).unwrap();
    let decoded: CreateSession = serde_json::from_value(session.clone()).unwrap();
    assert_eq!(decoded.model_config, Some(model_config()));
    session["secret"] = serde_json::json!("not allowed");
    assert!(serde_json::from_value::<CreateSession>(session).is_err());
    let update: UpdateSession = serde_json::from_str("{}").unwrap();
    assert!(update.model_config.is_none());
    let update: UpdateSession = serde_json::from_value(serde_json::json!({
        "model_config": model_config()
    }))
    .unwrap();
    assert_eq!(update.model_config, Some(model_config()));
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
        source: None,
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(!json.contains("source"));
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
fn paths_and_client_base_urls_are_validated() {
    assert_eq!(
        routes::agent_path(routes::AGENT_EVENTS, SessionId(1), AgentId(2)),
        "/sessions/1/agents/2/events"
    );
    assert_eq!(
        routes::agent_snapshot_path(AgentDefinitionId(42)),
        "/agent-snapshots/42"
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
        assert!(Client::new(invalid, "test-client-key").is_err());
    }
    assert!(Client::new("http://localhost:8080/prefix/", "test-client-key").is_ok());
}

#[test]
fn attributed_message_requests_round_trip() {
    let request = AgentRequest::Message {
        delivery: MessageDelivery::Steer,
        source: Some(AgentId(9)),
        content: vec![llm_api::api::ContentBlock::Text(llm_api::api::TextBlock {
            text: "task".into(),
            annotations: vec![],
        })],
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["Message"]["source"], 9);
    let AgentRequest::Message { source, .. } =
        serde_json::from_value::<AgentRequest>(encoded).unwrap()
    else {
        panic!()
    };
    assert_eq!(source, Some(AgentId(9)));
}

fn summary(name: &str, project: Option<&str>) -> Session {
    Session {
        id: SessionId(1),
        parent_id: None,
        name: name.into(),
        working_directory: "/work".into(),
        probe_id: ProbeId(probes::Uuid::nil()),
        project_id: project.map(|project| ProjectId(project.into())),
        model_config: model_config(),
        decision_model: None,
        busy: false,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn decision_call_events_round_trip_with_durations() {
    use llm_api::api::decisions::{
        DecisionAnswer, DecisionQuestion, DecisionRequest, DecisionResponse,
    };
    use std::{collections::BTreeMap, time::Duration};

    let model: llm_api::providers::Model = "openrouter/vendor/decider".parse().unwrap();
    let request = DecisionRequest {
        model: model.clone(),
        state: serde_json::json!({"ticket": "Where is my refund?"}),
        questions: BTreeMap::from([(
            "angry".into(),
            DecisionQuestion::Noul {
                instructions: "Is the customer angry?".into(),
                criteria: None,
            },
        )]),
    };
    let answered = DecisionCall {
        tool_call_id: Some("call-1".into()),
        request: request.clone(),
        response: Ok(DecisionResponse {
            model,
            response_id: Some("gen-dec-1".into()),
            answers: BTreeMap::from([
                (
                    "angry".into(),
                    DecisionAnswer::Noul {
                        noul: 0.25,
                        confidence: Some(0.9),
                    },
                ),
                ("other".into(), DecisionAnswer::Refusal),
            ]),
            usage: llm_api::api::Usage {
                input_tokens: Some(128),
                cost: Some(0.0000384),
                ..Default::default()
            },
            provider_metadata: Default::default(),
        }),
        duration: Duration::from_millis(1250),
    };
    let failed = DecisionCall {
        tool_call_id: None,
        request,
        response: Err("HTTP 429".into()),
        duration: Duration::from_millis(40),
    };
    for call in [answered, failed] {
        let event = events::EventType::DecisionCall(call.clone());
        let restored: events::EventType =
            serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert!(matches!(restored, events::EventType::DecisionCall(restored) if restored == call));
    }
}

#[test]
fn project_ids_are_trimmed_bounded_names_and_default_to_global() {
    assert!(ProjectId("pipeline".into()).validate().is_ok());
    for invalid in ["", "  ", " pipeline", &"x".repeat(ProjectId::MAX_LEN + 1)] {
        assert!(ProjectId(invalid.into()).validate().is_err(), "{invalid:?}");
    }

    let request: CreateSession = serde_json::from_value(serde_json::json!({
        "name": "s", "working_directory": "/work", "probe_id": probes::Uuid::nil(),
    }))
    .unwrap();
    assert!(request.project_id.is_none());
    let session = serde_json::to_value(summary("s", Some("pipeline"))).unwrap();
    assert_eq!(session["project_id"], "pipeline");
}

#[test]
fn session_filters_combine_criteria_and_treat_empty_lists_as_all() {
    let global = summary("Nightly Build", None);
    let project = summary("nightly deploy", Some("pipeline"));
    let filter = |name: &str, projects: Vec<Option<&str>>| SessionFilter {
        name_contains: name.into(),
        project_ids: projects
            .into_iter()
            .map(|project| project.map(|p| ProjectId(p.into())))
            .collect(),
        ..Default::default()
    };

    assert!(SessionFilter::default().is_empty());
    assert!(SessionFilter::default().matches(&global));
    assert!(filter("NIGHTLY", vec![]).matches(&project));
    assert!(!filter("build", vec![]).matches(&project));
    assert!(filter("", vec![None]).matches(&global));
    assert!(!filter("", vec![None]).matches(&project));
    assert!(filter("deploy", vec![None, Some("pipeline")]).matches(&project));
    assert!(!filter("build", vec![Some("pipeline")]).matches(&global));
    assert!(
        !SessionFilter {
            probe_ids: vec![ProbeId(probes::Uuid::from_u128(1))],
            ..Default::default()
        }
        .matches(&global)
    );
    assert!(serde_json::from_value::<SessionFilter>(serde_json::json!({"typo": 1})).is_err());
}
