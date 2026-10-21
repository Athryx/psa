use super::{
    Hook, HookKind, Hooks,
    tools::{Noop, Tool, ToolKind, tool_definition},
};
use crate::{
    agents::{Agent, AgentState, AgentUsage},
    sessions::Session,
};
use psa_api::{AgentDefinitionId, AgentId, HookId, HookIds, SessionId, ToolId, events::EventId};
use time::OffsetDateTime;

fn snapshots() -> (Agent, Session) {
    let now = OffsetDateTime::now_utc();
    let agent = Agent {
        interactive: false,
        id: AgentId(1),
        session_id: SessionId(1),
        definition_id: AgentDefinitionId(1),
        parent_id: None,
        first_event_id: EventId(1),
        next_event_id: EventId(1),
        created_position: 1,
        name: "test".into(),
        state: AgentState::Idle,
        usage: AgentUsage::default(),
        created_at: now,
        updated_at: now,
    };
    let session = Session {
        id: agent.session_id,
        parent_id: None,
        first_position: 1,
        next_position: 1,
        name: "test".into(),
        working_directory: "/tmp".into(),
        probe_id: crate::test_support::probe_metadata().id,
        model_config: crate::test_support::model_config(),
        created_at: now,
        updated_at: now,
    };
    (agent, session)
}

#[test]
fn hook_construction_preserves_configured_order_and_empty_lists() {
    let (agent, session) = snapshots();
    let hooks = Hooks::new(
        &HookIds {
            agent_create: vec![
                HookId::Noop,
                HookId::DefaultStartup,
                HookId::ExtraSystemPrompt,
                HookId::Cwd,
                HookId::ToolDefinitions,
                HookId::Skills,
            ],
            session_update: vec![HookId::SessionModelUpdate],
            before_request: vec![],
            after_request: vec![HookId::StopWithoutTools, HookId::Noop],
        },
        &agent,
        &session,
    );

    assert!(matches!(
        hooks.agent_create.as_slice(),
        [
            HookKind::Noop(_),
            HookKind::DefaultStartup(_),
            HookKind::ExtraSystemPrompt(_),
            HookKind::Cwd(_),
            HookKind::ToolDefinitions(_),
            HookKind::Skills(_),
        ]
    ));
    assert!(matches!(
        hooks.session_update.as_slice(),
        [HookKind::SessionModelUpdate(_)]
    ));
    assert!(hooks.before_request.is_empty());
    assert!(matches!(
        hooks.after_request.as_slice(),
        [HookKind::StopWithoutTools(_), HookKind::Noop(_)]
    ));

    let empty = Hooks::new(&HookIds::default(), &agent, &session);
    assert!(empty.agent_create.is_empty());
    assert!(empty.session_update.is_empty());
    assert!(empty.before_request.is_empty());
    assert!(empty.after_request.is_empty());
    let _ = super::NoopHook::new(&agent, &session);
}

#[test]
fn tool_instances_supply_the_fixture_definition_and_identity() {
    let (agent, session) = snapshots();
    let tool = ToolKind::new(ToolId::Noop, &agent, &session);
    let native = Noop::new(&agent, &session);

    assert_eq!(tool.id(), ToolId::Noop);
    let expected = serde_json::to_value(tool_definition(ToolId::Noop)).unwrap();
    assert_eq!(
        serde_json::to_value(tool.tool_definition()).unwrap(),
        expected
    );
    assert_eq!(
        serde_json::to_value(native.tool_definition()).unwrap(),
        expected
    );
    assert_eq!(native.system_prompt().tools, None);
    assert!(native.system_prompt().rules.is_empty());
    assert_eq!(tool.system_prompt(), native.system_prompt());
}
