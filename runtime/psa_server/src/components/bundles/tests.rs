use super::{testing::*, *};
use crate::{
    agents::{AgentState, AgentUsage},
    components::{
        hooks::{HookConfig, HookConfigs},
        tools::{ExecutionMode, Read, Tool, ToolId},
    },
};
use psa_api::{AgentDefinitionId, AgentId, SessionId, events::EventId};
use std::sync::atomic::Ordering;
use time::OffsetDateTime;

fn snapshots() -> (Agent, Session) {
    let now = OffsetDateTime::now_utc();
    let agent = Agent {
        interactive: false,
        hidden: false,
        id: AgentId(1),
        session_id: SessionId(1),
        definition_id: AgentDefinitionId(1),
        parent_id: None,
        subagent_parent_id: None,
        first_event_id: EventId(1),
        next_event_id: EventId(1),
        created_position: 1,
        name: "bundles".into(),
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
        name: "bundles".into(),
        working_directory: "/tmp".into(),
        probe_id: crate::test_support::probe_metadata().id,
        model_config: crate::test_support::model_config(),
        created_at: now,
        updated_at: now,
    };
    (agent, session)
}

fn definition() -> AgentDefinition {
    AgentDefinition {
        id: AgentDefinitionId(1),
        previous_version_id: None,
        version: 1,
        name: "bundles".into(),
        description: String::new(),
        selectable: false,
        bundles: Vec::new(),
        hooks: HookConfigs::default(),
        model_config: None,
        skills: Vec::new(),
        permitted_subagents: Vec::new(),
    }
}

fn probe(name: &str, state: &Arc<ProbeState>, cleanup: CleanupBehavior) -> BundleKind {
    let mut bundle = ProbeBundle::new(name, state.clone());
    bundle.cleanup = cleanup;
    bundle.timeout = Duration::from_millis(5);
    BundleKind::Probe(bundle)
}

#[tokio::test]
async fn bundle_order_and_empty_component_configuration_are_preserved() {
    let (agent, session) = snapshots();
    let mut empty = InstalledComponents::install(&definition(), &agent, &session)
        .await
        .unwrap();
    assert!(empty.tools.is_empty());
    empty.cleanup().await.unwrap();

    let mut definition = definition();
    definition.bundles = vec![
        BundleConfig::FileTools { read_only: false },
        BundleConfig::Noop,
    ];
    definition.hooks.before_request = vec![HookConfig::Noop, HookConfig::Noop];
    let mut components = InstalledComponents::install(&definition, &agent, &session)
        .await
        .unwrap();
    assert_eq!(
        components
            .tools
            .iter()
            .map(ToolKind::id)
            .collect::<Vec<_>>(),
        [ToolId::Read, ToolId::Edit, ToolId::Write, ToolId::Noop]
    );
    assert_eq!(components.hooks.before_request.len(), 2);
    components.cleanup().await.unwrap();
    assert!(components.tools.is_empty());
    assert!(components.hooks.before_request.is_empty());
}

#[tokio::test]
async fn bash_tools_are_installed_together_and_require_their_bundle() {
    let (agent, session) = snapshots();
    let mut definition = definition();
    definition.bundles = vec![BundleConfig::Bash];
    let mut components = InstalledComponents::install(&definition, &agent, &session)
        .await
        .unwrap();
    assert_eq!(
        components
            .tools
            .iter()
            .map(ToolKind::id)
            .collect::<Vec<_>>(),
        [ToolId::Bash, ToolId::InteractBash, ToolId::CloseBash]
    );
    components.cleanup().await.unwrap();
}

#[tokio::test]
async fn goal_installation_respects_interactive_only_and_feature_tools_require_bundles() {
    let (mut agent, session) = snapshots();
    let mut definition = definition();
    for interactive in [false, true] {
        agent.interactive = interactive;
        for interactive_only in [false, true] {
            definition.bundles = vec![BundleConfig::Goal { interactive_only }];
            let mut installed = InstalledComponents::install(&definition, &agent, &session)
                .await
                .unwrap();
            let enabled = interactive || !interactive_only;
            let expected = if enabled {
                vec![ToolId::SetGoal, ToolId::SetGoalStatus]
            } else {
                vec![]
            };
            assert_eq!(
                installed.tools.iter().map(ToolKind::id).collect::<Vec<_>>(),
                expected
            );
            assert!(
                installed
                    .tools
                    .iter()
                    .all(|tool| tool.execution_mode() == ExecutionMode::Sequential)
            );
            assert_eq!(installed.hooks.after_request.len(), 1);
            assert!(matches!(
                (&installed.hooks.after_request[0], enabled),
                (HookKind::Goal(_), true) | (HookKind::StopWithoutTools(_), false)
            ));
            installed.cleanup().await.unwrap();
        }
    }

    definition.bundles = vec![BundleConfig::Subagents];
    let error = InstalledComponents::install(&definition, &agent, &session)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("session bundle resources"));
}

#[tokio::test]
async fn ask_question_installation_respects_interactive_only_for_both_agent_modes() {
    let (mut agent, session) = snapshots();
    let mut definition = definition();
    for interactive in [false, true] {
        agent.interactive = interactive;
        for interactive_only in [false, true] {
            definition.bundles = vec![BundleConfig::AskQuestion { interactive_only }];
            let mut installed = InstalledComponents::install(&definition, &agent, &session)
                .await
                .unwrap();
            let expected = if interactive || !interactive_only {
                vec![ToolId::AskQuestion]
            } else {
                vec![]
            };
            assert_eq!(
                installed.tools.iter().map(ToolKind::id).collect::<Vec<_>>(),
                expected
            );
            assert!(installed.hooks.agent_create.is_empty());
            assert!(installed.hooks.session_update.is_empty());
            assert!(installed.hooks.before_request.is_empty());
            assert!(installed.hooks.after_request.is_empty());
            installed.cleanup().await.unwrap();
        }
    }
}

#[tokio::test]
async fn file_tools_read_only_selects_only_the_read_capability() {
    let (agent, session) = snapshots();
    for read_only in [false, true] {
        let mut definition = definition();
        definition.bundles = vec![BundleConfig::FileTools { read_only }];
        let mut installed = InstalledComponents::install(&definition, &agent, &session)
            .await
            .unwrap();
        let expected = if read_only {
            vec![ToolId::Read]
        } else {
            vec![ToolId::Read, ToolId::Edit, ToolId::Write]
        };
        assert_eq!(
            installed.tools.iter().map(ToolKind::id).collect::<Vec<_>>(),
            expected
        );
        installed.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn duplicate_bundle_ids_are_rejected_even_with_different_payloads() {
    let (agent, session) = snapshots();
    let mut definition = definition();
    definition.bundles = vec![
        BundleConfig::FileTools { read_only: false },
        BundleConfig::FileTools { read_only: true },
    ];
    let error = InstalledComponents::install(&definition, &agent, &session)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("duplicate bundle ID"));
    definition.bundles = vec![BundleConfig::Noop, BundleConfig::Noop];
    assert!(
        InstalledComponents::install(&definition, &agent, &session)
            .await
            .is_err()
    );
}

#[test]
fn removed_tools_fields_are_rejected_even_when_empty() {
    for tools in [
        serde_json::json!([]),
        serde_json::json!(["Noop"]),
        serde_json::json!(["Read"]),
        serde_json::json!(["Bash"]),
        serde_json::json!(["AskQuestion"]),
    ] {
        assert!(
            serde_json::from_value::<psa_api::CreateAgentDefinition>(
                serde_json::json!({"name":"legacy", "tools":tools})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<psa_api::AgentDefaults>(serde_json::json!({"tools":tools}))
                .is_err()
        );
        let config = serde_json::json!({"config_id":{"config_group":"Global","name":"legacy"},"models":["Global"],"tools":tools});
        assert!(serde_json::from_value::<psa_api::AgentConfig>(config.clone()).is_err());
        assert!(serde_json::from_value::<psa_api::AgentConfigInput>(config).is_err());
    }
}

#[test]
fn duplicate_installed_tool_ids_are_rejected_independently_of_api_name() {
    let mut installer = Installer::default();
    installer.add_tool(ToolKind::Read(Read)).unwrap();
    let error = installer
        .add_tool(ToolKind::BundleProbe(SharedTool {
            state: Arc::default(),
            id: ToolId::Read,
            name: "different-api-name".into(),
        }))
        .unwrap_err();
    assert!(error.message.contains("duplicate tool ID"));
}

#[test]
fn duplicate_api_names_are_rejected_independently_of_id() {
    let mut installer = Installer::default();
    let state = Arc::default();
    installer.add_tool(ToolKind::Read(Read)).unwrap();
    let error = installer
        .add_tool(ToolKind::BundleProbe(SharedTool {
            state,
            id: ToolId::Noop,
            name: "read".into(),
        }))
        .unwrap_err();
    assert!(error.message.contains("duplicate tool API name: read"));
}

#[test]
fn hook_registration_keeps_all_stages_and_order_without_deduplication() {
    let mut installer = Installer::default();
    let first = Arc::new(ProbeState::default());
    let second = Arc::new(ProbeState::default());
    for stage in [
        HookStage::AgentCreate,
        HookStage::SessionUpdate,
        HookStage::BeforeRequest,
        HookStage::AfterRequest,
    ] {
        installer.add_hook(
            stage,
            HookKind::BundleProbe(SharedHook {
                state: first.clone(),
            }),
        );
        installer.add_hook(
            stage,
            HookKind::BundleProbe(SharedHook {
                state: second.clone(),
            }),
        );
    }
    for hooks in [
        &installer.hooks.agent_create,
        &installer.hooks.session_update,
        &installer.hooks.before_request,
        &installer.hooks.after_request,
    ] {
        assert_eq!(hooks.len(), 2);
        let HookKind::BundleProbe(a) = &hooks[0] else {
            panic!("expected constructed hook")
        };
        let HookKind::BundleProbe(b) = &hooks[1] else {
            panic!("expected constructed hook")
        };
        assert!(Arc::ptr_eq(&a.state, &first));
        assert!(Arc::ptr_eq(&b.state, &second));
    }
}

#[tokio::test]
async fn constructed_tools_and_hook_share_the_bundle_state_and_drop_before_cleanup() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut bundle = ProbeBundle::new("shared", state.clone());
    bundle.components = true;
    let mut components = install_owned(vec![BundleKind::Probe(bundle)], &agent, &session)
        .await
        .unwrap();
    for tool in components.tools.iter() {
        let ToolKind::BundleProbe(tool) = tool else {
            panic!("expected constructed tool")
        };
        assert!(Arc::ptr_eq(&tool.state, &state));
        tool.increment();
    }
    let HookKind::BundleProbe(hook) = &components.hooks.before_request[0] else {
        panic!("expected constructed hook")
    };
    assert!(Arc::ptr_eq(&hook.state, &state));
    hook.increment();
    assert_eq!(state.value.load(Ordering::SeqCst), 3);
    components.cleanup().await.unwrap();
    assert_eq!(
        state.events(),
        [
            "tool_drop:probe_first",
            "tool_drop:probe_second",
            "hook_drop",
            "cleanup:shared",
            "drop:shared"
        ]
    );
    assert_eq!(Arc::strong_count(&state), 1);
}

#[tokio::test]
async fn cleanup_is_reverse_order_continues_after_failures_and_is_idempotent() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut components = install_owned(
        vec![
            probe("ok", &state, CleanupBehavior::Ok),
            probe("error", &state, CleanupBehavior::Error),
            probe("panic", &state, CleanupBehavior::Panic),
            probe("timeout", &state, CleanupBehavior::Pending),
        ],
        &agent,
        &session,
    )
    .await
    .unwrap();
    let errors = components.cleanup().await.unwrap_err().errors;
    assert_eq!(
        errors
            .iter()
            .map(|error| error.bundle.as_str())
            .collect::<Vec<_>>(),
        ["timeout", "panic", "error"]
    );
    assert!(matches!(
        errors[0].kind,
        CleanupFailureKind::TimedOut { .. }
    ));
    assert!(matches!(
        errors[1].kind,
        CleanupFailureKind::Panicked { .. }
    ));
    assert!(matches!(errors[2].kind, CleanupFailureKind::Failed { .. }));
    let events = state.events();
    assert_eq!(
        &events[..4],
        [
            "cleanup:timeout",
            "cleanup:panic",
            "cleanup:error",
            "cleanup:ok"
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("drop:"))
            .count(),
        4
    );
    components.cleanup().await.unwrap();
    assert_eq!(state.events(), events);
}

#[tokio::test]
async fn rollback_retains_primary_failure_and_cleans_the_failing_owner() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut failing = ProbeBundle::new("failing", state.clone());
    failing.components = true;
    failing.install = InstallBehavior::Error;
    failing.cleanup = CleanupBehavior::Error;
    let error = install_owned(
        vec![
            probe("earlier", &state, CleanupBehavior::Ok),
            BundleKind::Probe(failing),
        ],
        &agent,
        &session,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.message, "install:failing");
    let rollback = error.rollback_errors.unwrap();
    assert_eq!(rollback.errors.len(), 1);
    assert_eq!(rollback.errors[0].bundle, "failing");
    assert_eq!(
        state.events(),
        [
            "tool_drop:probe_first",
            "tool_drop:probe_second",
            "hook_drop",
            "cleanup:failing",
            "cleanup:earlier",
            "drop:earlier",
            "drop:failing"
        ]
    );
    assert_eq!(Arc::strong_count(&state), 1);
}

#[tokio::test]
async fn synchronous_install_panic_rolls_back_partial_components() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut failing = ProbeBundle::new("panic", state.clone());
    failing.components = true;
    failing.install = InstallBehavior::Panic;
    let error = install_owned(vec![BundleKind::Probe(failing)], &agent, &session)
        .await
        .err()
        .unwrap();
    assert!(
        error
            .message
            .contains("installation panicked: install:panic")
    );
    assert!(state.events().contains(&"cleanup:panic".into()));
    assert_eq!(Arc::strong_count(&state), 1);
}

#[tokio::test]
async fn dropping_cleanup_future_uses_concrete_drop_for_every_owner() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut pending = ProbeBundle::new("pending", state.clone());
    pending.timeout = Duration::from_secs(60);
    pending.cleanup = CleanupBehavior::Pending;
    let mut components = install_owned(
        vec![
            probe("unvisited", &state, CleanupBehavior::Ok),
            BundleKind::Probe(pending),
        ],
        &agent,
        &session,
    )
    .await
    .unwrap();
    {
        let cleanup = components.cleanup();
        tokio::pin!(cleanup);
        tokio::select! {
            _ = &mut cleanup => panic!("cleanup should remain pending"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    assert_eq!(
        state.events(),
        ["cleanup:pending", "drop:unvisited", "drop:pending"]
    );
    assert_eq!(Arc::strong_count(&state), 1);
    components.cleanup().await.unwrap();
}

#[tokio::test]
async fn codemode_registry_rejects_recursive_and_completion_tools() {
    use crate::components::codemode_functions::CodeModeFunctionKind;
    use serde_json::json;

    let (agent, session) = snapshots();
    let mut installer = Installer::default();
    let mut codemode = CodeModeBundle::new();
    let mut goal = GoalBundle::new(false);
    let mut result = TypedResultBundle::new(json!({"type": "object"}).into(), 3).unwrap();
    codemode.install(&agent, &session, &mut installer).unwrap();
    goal.install(&agent, &session, &mut installer).unwrap();
    result.install(&agent, &session, &mut installer).unwrap();

    let tools = std::mem::take(&mut installer.tools);
    assert_eq!(
        tools.iter().map(ToolKind::id).collect::<Vec<_>>(),
        [
            ToolId::CodeMode,
            ToolId::ResetCodeMode,
            ToolId::SetGoal,
            ToolId::SetGoalStatus,
            ToolId::RecordResult,
        ]
    );
    for tool in tools {
        let error = installer
            .add_codemode_function(CodeModeFunctionKind::Tool(tool))
            .unwrap_err();
        assert!(error.message.contains("codemode cannot expose"));
    }
    assert_eq!(installer.codemode_functions.len(), 1);
    assert_eq!(
        installer.codemode_functions[0].definition().name,
        "PRIOR_RESULTS"
    );
    installer
        .add_codemode_function(CodeModeFunctionKind::Tool(ToolKind::Read(Read)))
        .unwrap();
    assert_eq!(installer.codemode_functions.len(), 2);

    drop(installer);
    result.cleanup().await.unwrap();
    goal.cleanup().await.unwrap();
    codemode.cleanup().await.unwrap();
}

#[tokio::test]
async fn dropping_components_without_async_cleanup_still_drops_concrete_owners() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let components = install_owned(
        vec![probe("fallback", &state, CleanupBehavior::Ok)],
        &agent,
        &session,
    )
    .await
    .unwrap();
    drop(components);
    assert_eq!(state.events(), ["drop:fallback"]);
}
