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
    let identity = serde_json::json!({
        "provider": "Openrouter",
        "api_name": "vendor/model"
    });
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
fn agent_config_listing_query_names_the_refresh_session() {
    let query = RefreshAndListAgentConfigs {
        refresh_session_id: Some(SessionId(7)),
    };
    let wire = serde_json::json!({"refresh_session_id": 7});
    assert_eq!(serde_json::to_value(&query).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<RefreshAndListAgentConfigs>(wire)
            .unwrap()
            .refresh_session_id,
        Some(SessionId(7))
    );
    assert_eq!(
        serde_json::from_str::<RefreshAndListAgentConfigs>("{}")
            .unwrap()
            .refresh_session_id,
        None
    );
}

#[test]
fn skill_listing_query_names_the_refresh_session() {
    let query = RefreshAndListSkills {
        refresh_session_id: Some(SessionId(7)),
    };
    let wire = serde_json::json!({"refresh_session_id": 7});
    assert_eq!(serde_json::to_value(&query).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<RefreshAndListSkills>(wire)
            .unwrap()
            .refresh_session_id,
        Some(SessionId(7))
    );
    assert_eq!(
        serde_json::from_str::<RefreshAndListSkills>("{}")
            .unwrap()
            .refresh_session_id,
        None
    );
}

#[test]
fn configured_agent_requests_carry_typed_model_and_effort_selections() {
    for model in [
        None,
        Some(ModelSelection::Global),
        Some(ModelSelection::Parent),
        Some(ModelSelection::Fixed {
            model: "openrouter/vendor/model".into(),
            effort: EffortSelection::Fixed(Effort::High),
        }),
    ] {
        let request = NewConfiguredAgent {
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
            serde_json::from_value::<NewConfiguredAgent>(wire)
                .unwrap()
                .model,
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
            serde_json::from_value::<NewConfiguredAgent>(serde_json::json!({
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
fn definitions_have_flat_versioned_wire_types_and_default_empty_hooks() {
    let request: CreateAgentDefinition = serde_json::from_value(serde_json::json!({
        "name": "assistant",
    }))
    .unwrap();
    assert_eq!(request.name, "assistant");
    assert_eq!(request.hooks, HookConfigs::default());
    assert!(request.bundles.is_empty());
    assert_eq!(request.model, AgentModel::Global);
    assert!(request.skills.is_empty());
    assert!(request.permitted_subagents.is_empty());
    assert_eq!(
        serde_json::from_str::<HookConfigs>("{}").unwrap(),
        HookConfigs::default()
    );
    let hooks = HookConfigs {
        agent_create: vec![
            HookConfig::DefaultStartup,
            HookConfig::ExtraSystemPrompt {
                prompt: "Extra instructions".into(),
            },
            HookConfig::ProjectContext,
        ],
        session_update: vec![HookConfig::SessionModelUpdate],
        before_request: vec![HookConfig::Noop],
        after_request: vec![HookConfig::StopWithoutTools],
    };
    let definition = AgentDefinition {
        id: AgentDefinitionId(9),
        previous_version_id: Some(AgentDefinitionId(4)),
        version: 2,
        name: request.name,
        description: "Reusable assistant".into(),
        selectable: true,
        bundles: vec![BundleConfig::FileTools { read_only: false }],
        hooks: hooks.clone(),
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
        serde_json::json!([{"FileTools": {"read_only": false}}])
    );
    assert!(json.get("tools").is_none());
    assert_eq!(json["hooks"]["agent_create"][0], "DefaultStartup");
    assert_eq!(
        json["hooks"]["agent_create"][1],
        serde_json::json!({"ExtraSystemPrompt": {"prompt": "Extra instructions"}})
    );
    assert!(json.get("extra_system_prompt").is_none());
    assert_eq!(json["hooks"]["agent_create"][2], "ProjectContext");
    assert_eq!(json["skills"][0], "/skills/example");
    assert_eq!(json["permitted_subagents"][0], 3);
    assert!(json.get("config").is_none());
    let decoded: AgentDefinition = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(
        decoded.bundles,
        vec![BundleConfig::FileTools { read_only: false }]
    );
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("bundles");
    let legacy: AgentDefinition = serde_json::from_value(legacy).unwrap();
    assert!(legacy.bundles.is_empty());
    assert_eq!(decoded.hooks, hooks);
    assert_eq!(decoded.model, model_config().into());
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
        bundles: vec![BundleConfig::FileTools { read_only: false }],
        ..Default::default()
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        json["bundles"],
        serde_json::json!([{"FileTools": {"read_only": false}}])
    );
    assert!(json.get("tools").is_none());
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
fn session_models_are_optional_and_agents_reference_definitions() {
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
    let agent = CreateAgent {
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        definition_id: AgentDefinitionId(7),
        name: "agent".into(),
    };
    let mut json = serde_json::to_value(agent).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"definition_id": 7, "name": "agent", "interactive": false, "hidden": false, "subagent_parent_id": null})
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
            subagent_parent_id: None,
            definition_id: AgentDefinitionId(7),
            name: "agent".into(),
            interactive,
            hidden: false,
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
            subagent_parent_id: None,
            name: request.name,
            interactive,
            hidden: false,
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
fn agent_hidden_wire_field_defaults_false_and_round_trips_bools() {
    let create_json = serde_json::json!({"definition_id": 7, "name": "agent"});
    let configured_json = serde_json::json!({
        "config_id": {"config_group": "Global", "name": "worker"},
        "name": "agent"
    });
    let mut create: CreateAgent = serde_json::from_value(create_json.clone()).unwrap();
    let mut configured: NewConfiguredAgent =
        serde_json::from_value(configured_json.clone()).unwrap();
    assert!(!create.hidden);
    assert!(!configured.hidden);

    for hidden in [false, true] {
        create.hidden = hidden;
        configured.hidden = hidden;
        for request in [
            serde_json::to_value(&create).unwrap(),
            serde_json::to_value(&configured).unwrap(),
        ] {
            assert_eq!(request["hidden"], hidden);
        }
        assert_eq!(
            serde_json::from_value::<CreateAgent>(serde_json::to_value(&create).unwrap())
                .unwrap()
                .hidden,
            hidden
        );
        assert_eq!(
            serde_json::from_value::<NewConfiguredAgent>(
                serde_json::to_value(&configured).unwrap()
            )
            .unwrap()
            .hidden,
            hidden
        );

        let agent = Agent {
            id: AgentId(1),
            session_id: SessionId(2),
            definition_id: create.definition_id,
            parent_id: None,
            subagent_parent_id: None,
            name: create.name.clone(),
            interactive: false,
            hidden,
            state: AgentState::Idle,
            usage: AgentUsage::default(),
            next_event_id: events::EventId(1),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let update = serde_json::to_value(StreamMessage::AgentUpdated {
            agent: agent.clone(),
        })
        .unwrap();
        assert_eq!(update["agent"]["hidden"], hidden);
        let mut json = serde_json::to_value(&agent).unwrap();
        assert_eq!(json["hidden"], hidden);
        assert_eq!(
            serde_json::from_value::<Agent>(json.clone())
                .unwrap()
                .hidden,
            hidden
        );
        json.as_object_mut().unwrap().remove("hidden");
        assert!(
            !serde_json::from_value::<Agent>(json.clone())
                .unwrap()
                .hidden
        );

        for invalid in [
            serde_json::Value::Null,
            serde_json::json!("true"),
            serde_json::json!(1),
        ] {
            let mut request = create_json.clone();
            request["hidden"] = invalid.clone();
            assert!(serde_json::from_value::<CreateAgent>(request).is_err());
            let mut request = configured_json.clone();
            request["hidden"] = invalid.clone();
            assert!(serde_json::from_value::<NewConfiguredAgent>(request).is_err());
            json["hidden"] = invalid;
            assert!(serde_json::from_value::<Agent>(json.clone()).is_err());
        }
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
fn core_subagent_ancestry_and_attributed_requests_round_trip() {
    let legacy: CreateAgent = serde_json::from_value(serde_json::json!({
        "definition_id": 1, "name": "root"
    }))
    .unwrap();
    assert_eq!(legacy.subagent_parent_id, None);
    let child: CreateAgent = serde_json::from_value(serde_json::json!({
        "definition_id": 1, "name": "worker", "subagent_parent_id": 9
    }))
    .unwrap();
    assert_eq!(child.subagent_parent_id, Some(AgentId(9)));
    let request = AgentRequest::SubagentMessage {
        source: Some(AgentMessageSource {
            agent_id: Some(AgentId(9)),
            name: Some("parent".into()),
        }),
        content: vec![llm_api::api::ContentBlock::Text(llm_api::api::TextBlock {
            text: "task".into(),
            annotations: vec![],
        })],
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["SubagentMessage"]["source"]["agent_id"], 9);
    let AgentRequest::SubagentMessage { source, .. } =
        serde_json::from_value::<AgentRequest>(encoded).unwrap()
    else {
        panic!()
    };
    assert_eq!(source.unwrap().name.as_deref(), Some("parent"));
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
        busy: false,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
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
