use super::*;
use crate::codemode_functions::ToolFunction;
use crate::tools::{ExecutionMode, Read, ToolId};
use llm_api::api::Usage;
use psa::api::{AgentDefinitionId, AgentId, SessionId, events::EventId};
use psa_agents::bundles::testing::*;
use psa_agents::{AgentDefinition, AgentState};
use psa_store::{Agent, Session};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;

fn snapshots() -> (Agent, Session) {
    let now = OffsetDateTime::now_utc();
    let agent = Agent {
        interactive: false,
        hidden: false,
        id: AgentId(1),
        session_id: SessionId(1),
        definition_id: AgentDefinitionId(1),
        forked_from: None,
        subagent_parent_id: None,
        first_event_id: EventId(1),
        next_event_id: EventId(1),
        created_position: 1,
        name: "bundles".into(),
        state: AgentState::Idle,
        usage: Usage::zero(),
        cache: Default::default(),
        created_at: now,
        updated_at: now,
    };
    let session = Session {
        decision_model: None,
        id: agent.session_id,
        forked_from: None,
        first_position: 1,
        next_position: 1,
        name: "bundles".into(),
        working_directory: "/tmp".into(),
        probe_id: crate::test_support::probe_metadata().id,
        project_id: None,
        model_config: crate::test_support::model_config(),
        created_at: now,
        updated_at: now,
    };
    (agent, session)
}

fn definition() -> AgentDefinition {
    AgentDefinition {
        id: AgentDefinitionId(1),
        name: "bundles".into(),
        description: String::new(),
        selectable: false,
        bundles: Vec::new(),
        model: Default::default(),
        skills: Vec::new(),
        permitted_subagents: Vec::new(),
    }
}

fn probe(name: &str, state: &Arc<ProbeState>, cleanup: CleanupBehavior) -> Box<dyn DynBundle> {
    let mut bundle = ProbeBundle::new(name, state.clone());
    bundle.cleanup = cleanup;
    bundle.timeout = Duration::from_millis(5);
    Box::new(bundle)
}

#[tokio::test]
async fn bundle_order_and_empty_component_configuration_are_preserved() {
    let (agent, session) = snapshots();
    let mut empty = crate::bundles::install_bundles(&definition(), &agent, &session, None)
        .await
        .unwrap();
    assert!(empty.tools.is_empty());
    empty.cleanup().await.unwrap();

    let mut definition = definition();
    definition.bundles = vec![
        BundleConfig::FileTools { read_only: false },
        BundleConfig::StopWithoutTools,
    ];
    let mut components = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .unwrap();
    assert_eq!(
        components
            .tools
            .iter()
            .map(|tool| tool.id())
            .collect::<Vec<_>>(),
        [ToolId::Read, ToolId::Edit, ToolId::Write]
    );
    assert!(
        components
            .hooks
            .after_request
            .iter()
            .map(|hook| hook.name())
            .eq(["StopWithoutTools"])
    );
    components.cleanup().await.unwrap();
    assert!(components.tools.is_empty());
    assert!(components.hooks.after_request.is_empty());
}

#[tokio::test]
async fn creation_hooks_keep_bundle_list_order() {
    let (agent, session) = snapshots();
    let mut definition = definition();
    definition.bundles = vec![
        BundleConfig::Startup {
            base_prompt: true,
            project_context: true,
            cwd: true,
            skills: true,
        },
        BundleConfig::ExtraSystemPrompt {
            prompt: "Extra".into(),
        },
    ];
    let mut components = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .unwrap();
    assert!(
        components
            .hooks
            .agent_create
            .iter()
            .map(|hook| hook.name())
            .eq([
                "DefaultStartup",
                "ToolDefinitions",
                "ProjectContext",
                "Cwd",
                "Skills",
                "ExtraSystemPrompt"
            ])
    );
    assert!(
        components
            .hooks
            .session_update
            .iter()
            .map(|hook| hook.name())
            .eq(["SessionModelUpdate"])
    );
    assert!(components.hooks.after_request.is_empty());
    components.cleanup().await.unwrap();

    definition.bundles = vec![BundleConfig::Startup {
        base_prompt: true,
        project_context: false,
        cwd: false,
        skills: false,
    }];
    let mut components = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .unwrap();
    assert!(
        components
            .hooks
            .agent_create
            .iter()
            .map(|hook| hook.name())
            .eq(["DefaultStartup", "ToolDefinitions"])
    );
    components.cleanup().await.unwrap();
}

#[tokio::test]
async fn bash_tools_are_installed_together_and_require_their_bundle() {
    let (agent, session) = snapshots();
    let mut definition = definition();
    definition.bundles = vec![BundleConfig::Bash];
    let mut components = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .unwrap();
    assert_eq!(
        components
            .tools
            .iter()
            .map(|tool| tool.id())
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
            let mut installed =
                crate::bundles::install_bundles(&definition, &agent, &session, None)
                    .await
                    .unwrap();
            let enabled = interactive || !interactive_only;
            let expected = if enabled {
                vec![ToolId::SetGoal, ToolId::SetGoalStatus]
            } else {
                vec![]
            };
            assert_eq!(
                installed
                    .tools
                    .iter()
                    .map(|tool| tool.id())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(
                installed
                    .tools
                    .iter()
                    .all(|tool| tool.execution_mode() == ExecutionMode::Sequential)
            );
            assert_eq!(installed.hooks.after_request.len(), 1);
            let expected = if enabled { "Goal" } else { "StopWithoutTools" };
            assert_eq!(installed.hooks.after_request[0].name(), expected);
            installed.cleanup().await.unwrap();
        }
    }

    definition.bundles = vec![BundleConfig::Subagents];
    let error = crate::bundles::install_bundles(&definition, &agent, &session, None)
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
            let mut installed =
                crate::bundles::install_bundles(&definition, &agent, &session, None)
                    .await
                    .unwrap();
            let expected = if interactive || !interactive_only {
                vec![ToolId::AskQuestion]
            } else {
                vec![]
            };
            assert_eq!(
                installed
                    .tools
                    .iter()
                    .map(|tool| tool.id())
                    .collect::<Vec<_>>(),
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
        let mut installed = crate::bundles::install_bundles(&definition, &agent, &session, None)
            .await
            .unwrap();
        let expected = if read_only {
            vec![ToolId::Read]
        } else {
            vec![ToolId::Read, ToolId::Edit, ToolId::Write]
        };
        assert_eq!(
            installed
                .tools
                .iter()
                .map(|tool| tool.id())
                .collect::<Vec<_>>(),
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
    let error = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("duplicate bundle ID"));
    definition.bundles = vec![BundleConfig::Bash, BundleConfig::Bash];
    assert!(
        crate::bundles::install_bundles(&definition, &agent, &session, None)
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
            serde_json::from_value::<psa_store::CreateAgentDefinition>(
                serde_json::json!({"name":"legacy", "tools":tools})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<psa_config::server::AgentDefaults>(
                serde_json::json!({"tools":tools})
            )
            .is_err()
        );
        let config = serde_json::json!({"config_id":{"config_group":"Global","name":"legacy"},"models":["Global"],"tools":tools});
        assert!(serde_json::from_value::<psa::api::AgentConfig>(config.clone()).is_err());
        assert!(serde_json::from_value::<psa::api::AgentConfigInput>(config).is_err());
    }
}

#[test]
fn duplicate_installed_tool_ids_are_rejected_independently_of_api_name() {
    let mut installer = Installer::default();
    installer.add_tool(Read).unwrap();
    let error = installer
        .add_tool(SharedTool {
            state: Arc::default(),
            id: ToolId::Read,
            name: "different-api-name".into(),
        })
        .unwrap_err();
    assert!(error.message.contains("duplicate tool ID"));
}

#[test]
fn duplicate_api_names_are_rejected_independently_of_id() {
    let mut installer = Installer::default();
    let state = Arc::default();
    installer.add_tool(Read).unwrap();
    let error = installer
        .add_tool(SharedTool {
            state,
            id: ToolId::Edit,
            name: "read".into(),
        })
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
            SharedHook {
                state: first.clone(),
                name: "first",
            },
        );
        installer.add_hook(
            stage,
            SharedHook {
                state: second.clone(),
                name: "second",
            },
        );
    }
    for hooks in [
        &installer.hooks().agent_create,
        &installer.hooks().session_update,
        &installer.hooks().before_request,
        &installer.hooks().after_request,
    ] {
        assert!(hooks.iter().map(|hook| hook.name()).eq(["first", "second"]));
    }
}

#[tokio::test]
async fn constructed_tools_and_hook_share_the_bundle_state_and_drop_before_cleanup() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut bundle = ProbeBundle::new("shared", state.clone());
    bundle.components = true;
    let mut components = install_owned(vec![Box::new(bundle)], &agent, &session)
        .await
        .unwrap();
    // The test, the bundle, both tools, and the hook each hold the shared state.
    assert_eq!(components.tools.len(), 2);
    assert_eq!(components.hooks.before_request.len(), 1);
    assert_eq!(Arc::strong_count(&state), 5);
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
        ["timeout", "error"]
    );
    assert!(matches!(
        errors[0].kind,
        CleanupFailureKind::TimedOut { .. }
    ));
    assert!(matches!(errors[1].kind, CleanupFailureKind::Failed { .. }));
    let events = state.events();
    assert_eq!(
        &events[..3],
        ["cleanup:timeout", "cleanup:error", "cleanup:ok"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("drop:"))
            .count(),
        3
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
            Box::new(failing),
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
async fn dropping_cleanup_future_uses_concrete_drop_for_every_owner() {
    let (agent, session) = snapshots();
    let state = Arc::new(ProbeState::default());
    let mut pending = ProbeBundle::new("pending", state.clone());
    pending.timeout = Duration::from_secs(60);
    pending.cleanup = CleanupBehavior::Pending;
    let mut components = install_owned(
        vec![
            probe("unvisited", &state, CleanupBehavior::Ok),
            Box::new(pending),
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
    use serde_json::json;

    let (agent, session) = snapshots();
    let mut installer = Installer::default();
    let mut codemode = CodeModeBundle::new();
    let mut goal = GoalBundle::new(false);
    let mut result = TypedResultBundle::new(json!({"type": "object"}).into(), 3).unwrap();
    Bundle::install(&mut codemode, &agent, &session, &mut installer).unwrap();
    Bundle::install(&mut goal, &agent, &session, &mut installer).unwrap();
    Bundle::install(&mut result, &agent, &session, &mut installer).unwrap();

    let tools = installer.take_tools();
    assert_eq!(
        tools.iter().map(|tool| tool.id()).collect::<Vec<_>>(),
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
            .add_codemode_function(ToolFunction::from_dyn(tool))
            .unwrap_err();
        assert!(error.message.contains("codemode cannot expose"));
    }
    let names: Vec<_> = installer
        .codemode_functions()
        .iter()
        .map(|entry| entry.function.definition().name)
        .collect();
    assert_eq!(names, ["prior_results", "help"]);
    installer
        .add_codemode_function(ToolFunction::new(Read))
        .unwrap();
    assert_eq!(installer.codemode_functions().len(), 3);

    drop(installer);
    Bundle::cleanup(&mut result).await.unwrap();
    Bundle::cleanup(&mut goal).await.unwrap();
    Bundle::cleanup(&mut codemode).await.unwrap();
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

#[tokio::test]
async fn describe_attributes_components_to_their_bundles_in_order() {
    use psa::api::HookStage;

    let (agent, session) = snapshots();
    let mut definition = definition();
    definition.bundles = vec![
        BundleConfig::Startup {
            base_prompt: true,
            project_context: true,
            cwd: false,
            skills: false,
        },
        BundleConfig::FileTools { read_only: true },
        BundleConfig::Compaction {
            reserve_tokens: 1,
            keep_recent_tokens: 1,
            context_window: None,
        },
        BundleConfig::Goal {
            interactive_only: true,
        },
    ];
    let mut components = crate::bundles::install_bundles(&definition, &agent, &session, None)
        .await
        .unwrap();
    let described = components.describe();

    let configs: Vec<_> = described.bundles.iter().map(|b| b.config.clone()).collect();
    assert_eq!(configs, definition.bundles);

    let [startup, files, compaction, goal] = &described.bundles[..] else {
        panic!("one entry per bundle");
    };
    assert!(startup.tools.is_empty());
    assert!(
        startup
            .hooks
            .iter()
            .all(|hook| hook.stage != HookStage::Command)
    );
    assert!(
        startup
            .hooks
            .iter()
            .any(|hook| hook.name == "ProjectContext")
    );
    assert!(!startup.hooks.iter().any(|hook| hook.name == "Cwd"));

    let tools: Vec<_> = files.tools.iter().map(|tool| tool.id).collect();
    assert_eq!(tools, [ToolId::Read]);
    assert!(!files.tools[0].description.is_empty());
    assert!(files.hooks.is_empty());

    assert!(
        compaction
            .hooks
            .iter()
            .any(|hook| hook.stage == HookStage::Command && hook.name == "Compaction")
    );
    assert!(
        compaction
            .hooks
            .iter()
            .any(|hook| hook.stage == HookStage::BeforeRequest)
    );

    // Interactive-only goal mode installs only its stopping policy for a
    // noninteractive agent, never its tools or command handler.
    assert!(goal.tools.is_empty());
    assert_eq!(goal.hooks.len(), 1);
    assert_eq!(goal.hooks[0].name, "StopWithoutTools");
    assert!(
        goal.hooks
            .iter()
            .all(|hook| hook.stage != HookStage::Command)
    );

    let total: usize = described.bundles.iter().map(|b| b.tools.len()).sum();
    assert_eq!(total, components.tools.len());
    components.cleanup().await.unwrap();
}
