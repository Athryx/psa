use super::*;
use crate::{
    agents::{
        NewAgent,
        agent_manager::AgentManagerState,
        client_queries::ClientQueries,
        publication::Publication,
        runtime_handle::RuntimeHandle,
        supervisor::{RuntimeError, SupervisorClient, SupervisorCommand},
    },
    components::bundles::session::SessionBundleResources,
};
use llm_api::api::ContentBlock;
use psa::api::{AgentRequest, RequestAccepted};
use psa_store::{Database, NewSession};
use std::{future::Future, pin::Pin};

const WAIT: Duration = Duration::from_secs(2);

struct Fixture {
    _directory: tempfile::TempDir,
    database: Database,
    queries: ClientQueries,
    sessions: RuntimeHandle,
    manager: actors::Mailbox<SupervisorCommand>,
    state: AgentManagerState,
    resources: SessionBundleResources,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("create-agent.sqlite3"))
            .await
            .unwrap();
        crate::test_support::register_probe(&database).await;
        let mut cursor = database.write().await.unwrap();
        let worker = cursor
            .create_agent_definition(psa_store::CreateAgentDefinition {
                name: "worker-template".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let definition = cursor
            .create_agent_definition(psa_store::CreateAgentDefinition {
                name: "caller".into(),
                permitted_subagents: vec![worker.id],
                ..Default::default()
            })
            .await
            .unwrap();
        let session = cursor
            .create_session(NewSession {
                decision_model: None,
                name: "tool cancellation".into(),
                working_directory: "/work".into(),
                probe_id: crate::test_support::probe_metadata().id,
                project_id: None,
                model_config: crate::test_support::model_config(),
            })
            .await
            .unwrap();
        let caller = cursor
            .create_agent(
                session.id,
                NewAgent {
                    name: "caller".into(),
                    definition_id: definition.id,
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                },
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let publication = Publication::default();
        let state =
            AgentManagerState::load_persisted(database.clone(), publication.clone(), caller.id)
                .await
                .unwrap();
        let queries = ClientQueries::start(publication.clone());
        let (commands, manager) = actors::channel(32);
        let sessions = RuntimeHandle::new(
            database.clone(),
            publication,
            SupervisorClient::new(commands),
            queries.handle(),
        );
        let resources = SessionBundleResources::start(session.id, sessions.clone(), None);
        Self {
            _directory: directory,
            database,
            queries,
            sessions,
            manager,
            state,
            resources,
        }
    }

    async fn create_agent_tool(&self) -> CreateAgent {
        let handle = self.resources.handle().subagents();
        let templates = handle
            .load_child_templates(&self.state.definition().permitted_subagents)
            .await
            .unwrap();
        CreateAgent::new(handle, templates.into())
    }

    async fn close(mut self) {
        self.resources.cleanup().await.unwrap();
        self.queries.shutdown().await.unwrap();
        self.database.close().await;
    }
}

fn arguments() -> serde_json::Value {
    json!({"name":"worker-one","task":"do work","agent_template":"worker-template"})
}

fn output_text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

async fn next_command<F: Future<Output = ToolOutput>>(
    run: Pin<&mut F>,
    manager: &mut actors::Mailbox<SupervisorCommand>,
) -> SupervisorCommand {
    tokio::time::timeout(WAIT, async {
        tokio::select! {
            biased;
            output = run => panic!("tool returned before the expected manager command: {}", output_text(&output)),
            command = manager.recv() => command.expect("manager mailbox closed"),
        }
    }).await.expect("tool did not reach the expected manager command")
}

#[derive(Clone, Copy)]
enum CancelAt {
    AgentsCreated,
    InitialSubmission,
}

async fn cancelled_creation(cancel_at: CancelAt, fail_submission: bool, fail_interrupt: bool) {
    let mut fixture = Fixture::new().await;
    let caller = fixture.state.agent().id;
    let session = fixture.state.session().id;
    let tool = fixture.create_agent_tool().await;
    {
        let mut context =
            AgentContext::with_optional_probe(&mut fixture.state, None, &fixture.sessions);
        let (cancel, cancellation) = Cancellation::channel();
        let mut cancel = Some(cancel);
        let run = tool.execute(arguments(), &mut context, cancellation);
        tokio::pin!(run);
        let SupervisorCommand::AgentsCreated { agents } =
            next_command(run.as_mut(), &mut fixture.manager).await
        else {
            panic!("expected child creation notification");
        };
        assert_eq!(agents.len(), 1);
        let child = agents[0];
        if matches!(cancel_at, CancelAt::AgentsCreated) {
            cancel.take().unwrap().send(()).unwrap();
        }

        let SupervisorCommand::Submit {
            session: owner,
            agent,
            request,
            reply,
        } = next_command(run.as_mut(), &mut fixture.manager).await
        else {
            panic!("expected initial task submission, even after cancellation");
        };
        assert_eq!(owner, session);
        assert_eq!(agent, child);
        let AgentRequest::Message {
            source, content, ..
        } = request
        else {
            panic!("expected attributed initial task");
        };
        assert_eq!(source, Some(caller));
        assert!(matches!(&content[..], [ContentBlock::Text(text)] if text.text == "do work"));
        if matches!(cancel_at, CancelAt::InitialSubmission) {
            cancel.take().unwrap().send(()).unwrap();
        }
        assert!(
            futures_util::poll!(run.as_mut()).is_pending(),
            "cancellation must not abandon an in-flight submission"
        );
        assert!(
            !reply.is_closed(),
            "initial task reply must still have a receiver"
        );
        assert!(matches!(
            fixture.manager.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let submitted = if fail_submission {
            Err(RuntimeError::Invalid("initial task rejected".into()))
        } else {
            Ok(RequestAccepted {
                request_id: "initial-task".into(),
            })
        };
        reply.send(submitted).unwrap();

        let SupervisorCommand::Submit {
            session: owner,
            agent,
            request,
            reply,
        } = next_command(run.as_mut(), &mut fixture.manager).await
        else {
            panic!("expected interruption after the initial task reply");
        };
        assert_eq!(owner, session);
        assert_eq!(
            agent, child,
            "cancel cleanup must target the child it just created"
        );
        assert!(matches!(request, AgentRequest::Interrupt));
        let interrupted = if fail_interrupt {
            Err(RuntimeError::Invalid("interruption refused".into()))
        } else {
            Ok(RequestAccepted {
                request_id: "interrupt-child".into(),
            })
        };
        reply.send(interrupted).unwrap();
        let output = tokio::time::timeout(WAIT, run.as_mut()).await.unwrap();
        assert!(output.error);
        let text = output_text(&output);
        assert!(text.contains("worker-one"), "child name missing: {text}");
        if fail_interrupt {
            assert!(
                text.contains("interruption refused"),
                "interruption error missing: {text}"
            );
        } else {
            assert!(
                text.contains("has been interrupted"),
                "interruption outcome missing: {text}"
            );
        }
        let persisted = fixture.sessions.get_agent(session, child).await.unwrap();
        assert_eq!(persisted.subagent_parent_id, Some(caller));
        assert_eq!(persisted.name, "worker-one");
        assert_eq!(
            fixture.sessions.list_agents(session).await.unwrap().len(),
            2
        );
        assert!(matches!(
            fixture.manager.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
    fixture.close().await;
}

#[tokio::test]
async fn cancellation_after_creation_or_during_submission_interrupts_the_same_child() {
    for stage in [CancelAt::AgentsCreated, CancelAt::InitialSubmission] {
        cancelled_creation(stage, false, false).await;
    }
}

#[tokio::test]
async fn cancelled_creation_reports_child_identity_when_interrupt_fails() {
    cancelled_creation(CancelAt::InitialSubmission, false, true).await;
}

#[tokio::test]
async fn cancelled_creation_still_interrupts_when_initial_submission_fails() {
    cancelled_creation(CancelAt::InitialSubmission, true, false).await;
}

#[tokio::test]
async fn uncancelled_creation_reports_submission_result_without_interrupting() {
    for fail_submission in [false, true] {
        let mut fixture = Fixture::new().await;
        let tool = fixture.create_agent_tool().await;
        {
            let mut context =
                AgentContext::with_optional_probe(&mut fixture.state, None, &fixture.sessions);
            let (_cancel, cancellation) = Cancellation::channel();
            let run = tool.execute(arguments(), &mut context, cancellation);
            tokio::pin!(run);
            let SupervisorCommand::AgentsCreated { agents } =
                next_command(run.as_mut(), &mut fixture.manager).await
            else {
                panic!("expected child creation notification");
            };
            let child = agents[0];
            let SupervisorCommand::Submit {
                agent,
                request,
                reply,
                ..
            } = next_command(run.as_mut(), &mut fixture.manager).await
            else {
                panic!("expected initial task submission");
            };
            assert_eq!(agent, child);
            assert!(matches!(
                request,
                AgentRequest::Message {
                    source: Some(_),
                    ..
                }
            ));
            let result = if fail_submission {
                Err(RuntimeError::Invalid("initial task rejected".into()))
            } else {
                Ok(RequestAccepted {
                    request_id: "initial-task".into(),
                })
            };
            reply.send(result).unwrap();
            let output = tokio::time::timeout(WAIT, run.as_mut()).await.unwrap();
            assert_eq!(output.error, fail_submission);
            let text = output_text(&output);
            if fail_submission {
                assert!(text.contains("worker-one"));
                assert!(text.contains("initial task rejected"));
            } else {
                assert_eq!(
                    text,
                    "Started agent \"worker-one\". Its final answer will arrive as a message."
                );
            }
            assert!(matches!(
                fixture.manager.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn pre_cancelled_creation_has_no_child_or_manager_side_effects() {
    let mut fixture = Fixture::new().await;
    let session = fixture.state.session().id;
    let tool = fixture.create_agent_tool().await;
    let (cancel, cancellation) = Cancellation::channel();
    cancel.send(()).unwrap();
    let output = {
        let mut context =
            AgentContext::with_optional_probe(&mut fixture.state, None, &fixture.sessions);
        tool.execute(arguments(), &mut context, cancellation).await
    };
    assert!(output.error);
    assert_eq!(
        fixture.sessions.list_agents(session).await.unwrap().len(),
        1
    );
    assert!(matches!(
        fixture.manager.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    fixture.close().await;
}
