use super::*;
use crate::{
    agents::{Agent, AgentDefinition, AgentState},
    components::bundles::{BundleConfig, InstalledComponents},
    sessions::Session,
};
use llm_api::api::Usage;
use psa::api::{AgentDefinitionId, AgentId, SessionId, events::EventId};
use time::OffsetDateTime;

pub(super) fn snapshots() -> (AgentDefinition, Agent, Session) {
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
        name: "test".into(),
        state: AgentState::Idle,
        usage: Usage::zero(),
        created_at: now,
        updated_at: now,
    };
    let session = Session {
        decision_model: None,
        id: agent.session_id,
        parent_id: None,
        first_position: 1,
        next_position: 1,
        name: "test".into(),
        working_directory: "/tmp".into(),
        probe_id: crate::test_support::probe_metadata().id,
        project_id: None,
        model_config: crate::test_support::model_config(),
        created_at: now,
        updated_at: now,
    };
    let definition = AgentDefinition {
        id: AgentDefinitionId(1),
        previous_version_id: None,
        version: 1,
        name: "test".into(),
        description: String::new(),
        selectable: false,
        bundles: vec![],
        model: Default::default(),
        skills: vec![],
        permitted_subagents: vec![],
    };
    (definition, agent, session)
}

#[tokio::test]
async fn registry_follows_bundle_installation_and_cleanup() {
    let (mut definition, agent, session) = snapshots();
    definition.bundles = vec![
        BundleConfig::FileTools { read_only: true },
        BundleConfig::Bash,
        BundleConfig::CodeMode,
        BundleConfig::AskQuestion {
            interactive_only: true,
        },
    ];
    let mut components = InstalledComponents::install(&definition, &agent, &session)
        .await
        .unwrap();
    let names: Vec<_> = components
        .codemode_functions
        .iter()
        .map(|f| f.function.definition().name)
        .collect();
    assert_eq!(
        names,
        [
            "read",
            "bash",
            "interact_bash",
            "close_bash",
            "prior_results",
            "help"
        ]
    );
    components.cleanup().await.unwrap();
    assert!(components.codemode_functions.is_empty());
}

#[tokio::test]
async fn writable_files_and_interactive_question_are_installed() {
    let (mut definition, mut agent, session) = snapshots();
    agent.interactive = true;
    definition.bundles = vec![
        BundleConfig::FileTools { read_only: false },
        BundleConfig::AskQuestion {
            interactive_only: true,
        },
    ];
    let mut components = InstalledComponents::install(&definition, &agent, &session)
        .await
        .unwrap();
    let names: Vec<_> = components
        .codemode_functions
        .iter()
        .map(|f| f.function.definition().name)
        .collect();
    assert_eq!(names, ["read", "edit", "write", "ask_question"]);
    components.cleanup().await.unwrap();
}
