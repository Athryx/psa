use super::*;
use psa::api::{BundleConfig, RequestAccepted};
use psa_agents::{
    NewAgent, bundles::testing::TestComponents, supervisor::testing::captured_runtime,
};
use psa_store::{CreateAgentDefinition, Database, NewSession};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn completion_hook_reports_children_without_bundle_and_preserves_outcomes() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("completion.sqlite3"))
        .await
        .unwrap();
    crate::test_support::register_probe(&database).await;
    let mut cursor = database.write().await.unwrap();
    let parent_definition = cursor
        .create_agent_definition(CreateAgentDefinition {
            name: "parent".into(),
            bundles: vec![BundleConfig::Subagents],
            ..Default::default()
        })
        .await
        .unwrap();
    let child_definition = cursor
        .create_agent_definition(CreateAgentDefinition {
            name: "plain child".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            name: "completion".into(),
            working_directory: "/work".into(),
            probe_id: crate::test_support::probe_metadata().id,
            project_id: None,
            model_config: crate::test_support::model_config(),
        })
        .await
        .unwrap();
    let parent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                name: "parent".into(),
                definition_id: parent_definition.id,
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    let child = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: Some(parent.id),
                definition_id: child_definition.id,
                name: "child".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let (sessions, mut commands) = captured_runtime(database.clone(), Arc::new(TestComponents));

    for (outcome, expected) in [
        (AgentRunOutcome::Completed, "answer"),
        (
            AgentRunOutcome::Interrupted,
            "Interrupted before finishing.",
        ),
        (
            AgentRunOutcome::Failed {
                message: "failure".into(),
            },
            "Failed: failure",
        ),
    ] {
        let report = SubagentCompletion.run(
            &sessions,
            AgentLifecycleEvent::Finished {
                session_id: session.id,
                agent_id: child.id,
                run_id: "run".into(),
                outcome,
                final_text: Some("answer".into()),
            },
        );
        let receive = async {
            let steer = commands.next_steer().await;
            assert_eq!(steer.session, session.id);
            assert_eq!(steer.agent, parent.id);
            let AgentRequest::Message {
                source, content, ..
            } = steer.request
            else {
                panic!("expected attributed message")
            };
            assert_eq!(source, Some(child.id));
            assert!(
                matches!(&content[0], llm_api::api::ContentBlock::Text(text) if text.text == expected)
            );
            steer
                .reply
                .send(Ok(RequestAccepted {
                    request_id: "report".into(),
                }))
                .unwrap();
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(report, receive);
        })
        .await
        .unwrap();
    }
    database.close().await;
}
