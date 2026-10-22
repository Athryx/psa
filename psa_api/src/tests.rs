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
fn paths_and_client_base_urls_are_validated() {
    assert_eq!(
        routes::agent_path(routes::AGENT_EVENTS, SessionId(1), AgentId(2)),
        "/sessions/1/agents/2/events"
    );
    assert_eq!(
        routes::agent_definition_path(AgentDefinitionId(42)),
        "/agent-definitions/42"
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
