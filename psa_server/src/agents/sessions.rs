//! Session and agent operations shared by HTTP clients, tools, and hooks.
//!
//! Returns server-native snapshots; HTTP handlers build wire representations.
//!
//! Database transactions and publication guards are released before awaiting the
//! session manager, which owns request queues and worker lifecycles.

use super::{
    Agent, AgentError, NewAgent, ToolDefinitionExt,
    agent_manager::build_recovery_events,
    agent_projection::AgentProjection,
    publication::Publication,
    session_manager::{ManagerClient, RuntimeError},
};
use crate::{db::Database, sessions};
use psa_api::{self as api, AgentId, SessionId, events::*};

#[derive(Clone)]
pub struct Sessions {
    database: Database,
    publication: Publication,
    pub(crate) commands: ManagerClient,
}

impl Sessions {
    pub(crate) fn new(
        database: Database,
        publication: Publication,
        commands: ManagerClient,
    ) -> Self {
        Self {
            database,
            publication,
            commands,
        }
    }

    pub async fn create_session(
        &self,
        request: api::CreateSession,
    ) -> Result<sessions::Session, RuntimeError> {
        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let session = cursor
            .create_session(sessions::NewSession {
                name: request.name,
                working_directory: request.working_directory,
                probe_id: request.probe_id,
            })
            .await?;
        cursor.commit().await?;

        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(false),
        });
        Ok(session)
    }

    pub async fn list_sessions(
        &self,
        query: api::ListSessions,
    ) -> Result<Vec<sessions::Session>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let sessions = cursor.list_sessions(query.parent).await?;
        cursor.rollback().await?;

        Ok(sessions)
    }

    pub async fn get_session(&self, id: SessionId) -> Result<sessions::Session, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let session = cursor.get_session(id).await?;
        cursor.rollback().await?;

        Ok(session)
    }

    pub async fn update_session(
        &self,
        id: SessionId,
        request: api::UpdateSession,
    ) -> Result<sessions::Session, RuntimeError> {
        let busy = self.is_busy(id).await?;

        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let session = cursor
            .update_session(
                id,
                sessions::SessionUpdate {
                    name: request.name,
                    working_directory: request.working_directory,
                },
            )
            .await?;
        cursor.commit().await?;

        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(busy),
        });
        drop(publication);

        self.commands
            .session_updated(session.clone())
            .await
            .map_err(|_| RuntimeError::Unavailable)?;
        Ok(session)
    }

    pub async fn fork_session(
        &self,
        id: SessionId,
        request: api::ForkSession,
    ) -> Result<sessions::ForkedSession, RuntimeError> {
        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let fork = cursor
            .fork_session(
                id,
                sessions::SessionFork {
                    point: match request.point {
                        api::ForkPoint::Head => sessions::ForkPoint::Head,
                        api::ForkPoint::ThroughEvent { agent_id, event_id } => {
                            sessions::ForkPoint::ThroughEvent { agent_id, event_id }
                        }
                    },
                    metadata: sessions::SessionUpdate {
                        name: request.metadata.name,
                        working_directory: request.metadata.working_directory,
                    },
                },
            )
            .await?;

        let mut agents = Vec::new();
        let mut committed_messages = Vec::new();
        for (parent_id, agent) in fork.agents {
            let projection = AgentProjection::replay(&cursor.load_all_events(agent.id).await?)
                .map_err(AgentError::from)?;
            let events = build_recovery_events(&agent, &projection)?;
            let result = cursor.add_events(agent.id, &events).await?;
            let first_position = cursor.get_session(agent.session_id).await?.next_position
                - result.events.len() as u64;
            committed_messages.extend(result.events.into_iter().enumerate().map(
                |(offset, event)| api::StreamMessage::Committed {
                    event: api::EventEnvelope {
                        session_id: agent.session_id,
                        agent_id: agent.id,
                        position: first_position + offset as u64,
                        event,
                    },
                    pending_id: None,
                },
            ));
            agents.push((parent_id, cursor.get_agent(agent.id).await?));
        }

        let session = cursor.get_session(fork.session.id).await?;
        cursor.commit().await?;

        for message in committed_messages {
            publication.publish(message);
        }
        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(false),
        });
        Ok(sessions::ForkedSession { session, agents })
    }

    pub async fn create_agent(
        &self,
        session: SessionId,
        request: api::CreateAgent,
    ) -> Result<Agent, RuntimeError> {
        let mut history = llm_api::api::AgentHistory::new(request.model.clone());
        history.tools = request
            .tools
            .iter()
            .map(|id| crate::hooks::tools::tool_definition(*id))
            .collect();
        llm_api::api::validate_request(&history, request.model.provider, &Default::default())
            .map_err(|error| RuntimeError::Invalid(error.to_string()))?;

        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let agent = cursor
            .create_agent(session, NewAgent { name: request.name })
            .await?;
        let mut events = vec![NewEvent::new(EventType::Model(request.model))];
        events.extend(
            request
                .tools
                .into_iter()
                .map(|id| NewEvent::new(EventType::ToolDefinition(ToolDefinition::for_tool(id)))),
        );
        let result = cursor.add_events(agent.id, &events).await?;
        AgentProjection::replay(&result.events).map_err(AgentError::from)?;
        let agent = cursor.get_agent(agent.id).await?;
        let first_position =
            cursor.get_session(agent.session_id).await?.next_position - result.events.len() as u64;
        cursor.commit().await?;

        for (offset, event) in result.events.into_iter().enumerate() {
            publication.publish(api::StreamMessage::Committed {
                event: api::EventEnvelope {
                    session_id: agent.session_id,
                    agent_id: agent.id,
                    position: first_position + offset as u64,
                    event,
                },
                pending_id: None,
            });
        }
        publication.publish(api::StreamMessage::AgentUpdated {
            agent: agent.clone().into(),
        });
        Ok(agent)
    }

    pub async fn list_agents(&self, session: SessionId) -> Result<Vec<Agent>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let agents = cursor.list_agents(session).await?;
        cursor.rollback().await?;

        Ok(agents)
    }

    pub async fn get_agent(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Agent, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let agent = cursor.require_agent_session(session, agent).await?;
        cursor.rollback().await?;

        Ok(agent)
    }

    pub async fn is_busy(&self, session: SessionId) -> Result<bool, RuntimeError> {
        self.commands
            .is_busy(session)
            .await
            .map_err(|_| RuntimeError::Unavailable)
    }

    pub async fn submit(
        &self,
        session: SessionId,
        agent: AgentId,
        request: api::AgentRequest,
    ) -> Result<api::RequestAccepted, RuntimeError> {
        if matches!(&request, api::AgentRequest::Message { content, .. } if content.is_empty()) {
            return Err(RuntimeError::Invalid(
                "message content must not be empty".into(),
            ));
        }

        self.commands
            .submit(session, agent, request)
            .await
            .map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn steer(
        &self,
        session: SessionId,
        agent: AgentId,
        content: Vec<llm_api::api::ContentBlock>,
    ) -> Result<api::RequestAccepted, RuntimeError> {
        self.submit(
            session,
            agent,
            api::AgentRequest::Message {
                delivery: api::MessageDelivery::Steer,
                content,
            },
        )
        .await
    }

    pub async fn resume(
        &self,
        session: SessionId,
        agent: AgentId,
        content: Vec<llm_api::api::ContentBlock>,
    ) -> Result<api::RequestAccepted, RuntimeError> {
        self.submit(
            session,
            agent,
            api::AgentRequest::Message {
                delivery: api::MessageDelivery::Resume,
                content,
            },
        )
        .await
    }

    pub async fn interrupt(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<api::RequestAccepted, RuntimeError> {
        self.submit(session, agent, api::AgentRequest::Interrupt)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agents::session_manager::ManagerCommand, db::DbError};
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
    use std::time::Duration;

    struct Fixture {
        _directory: tempfile::TempDir,
        sessions: Sessions,
        received: actors::Mailbox<ManagerCommand>,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let database = Database::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            crate::test_support::register_probe(&database).await;
            let (commands, received) = actors::channel(1);
            Self {
                _directory: directory,
                sessions: Sessions::new(
                    database,
                    Publication::default(),
                    ManagerClient::new(commands),
                ),
                received,
            }
        }

        async fn create_session(&self) -> sessions::Session {
            self.sessions
                .create_session(api::CreateSession {
                    name: "test".into(),
                    working_directory: "/work".into(),
                    probe_id: api::ProbeId(api::probes::Uuid::nil()),
                })
                .await
                .unwrap()
        }
    }

    fn create_agent_request() -> api::CreateAgent {
        api::CreateAgent {
            name: "agent".into(),
            model: ModelConfig {
                model: Model::new("test-model").unwrap(),
                provider: ProviderType::Openrouter,
                effort: Effort::Medium,
            },
            tools: vec![api::ToolId::Noop],
        }
    }

    #[tokio::test]
    async fn creates_agents_and_forks_with_recovery_and_publication() {
        let fixture = Fixture::new().await;
        let sessions = &fixture.sessions;
        let mut published = sessions.publication.lock().await.subscribe();
        let session = fixture.create_session().await;
        assert_eq!(session.first_position, 1);
        assert_eq!(session.next_position, 1);
        assert!(
            matches!(published.recv().await.unwrap(), api::StreamMessage::SessionUpdated { session: value } if value.id == session.id)
        );

        let agent: crate::agents::Agent = sessions
            .create_agent(session.id, create_agent_request())
            .await
            .unwrap();
        assert_eq!(agent.created_position, 1);
        assert_eq!(agent.first_event_id, EventId(1));
        assert_eq!(agent.next_event_id, EventId(3));
        let listed: Vec<crate::agents::Agent> = sessions.list_agents(session.id).await.unwrap();
        assert_eq!(listed[0].id, agent.id);
        assert_eq!(listed[0].created_position, agent.created_position);
        assert_eq!(listed[0].first_event_id, agent.first_event_id);
        let loaded: crate::agents::Agent = sessions.get_agent(session.id, agent.id).await.unwrap();
        assert_eq!(loaded.name, "agent");
        assert_eq!(loaded.created_position, agent.created_position);
        assert_eq!(loaded.first_event_id, agent.first_event_id);
        for position in 2..=3 {
            let api::StreamMessage::Committed { event, pending_id } =
                published.recv().await.unwrap()
            else {
                panic!("expected initial model/tool event");
            };
            assert_eq!(event.position, position);
            assert_eq!(event.agent_id, agent.id);
            assert!(pending_id.is_none());
        }
        assert!(
            matches!(published.recv().await.unwrap(), api::StreamMessage::AgentUpdated { agent: value } if value.id == agent.id)
        );

        let mut cursor = sessions.database.cursor().await.unwrap();
        cursor
            .add_events(
                agent.id,
                &[NewEvent::new(EventType::StatusChange(StatusChange {
                    new_status: api::AgentState::Running,
                    reason: None,
                }))],
            )
            .await
            .unwrap();
        cursor.commit().await.unwrap();
        let fork = sessions
            .fork_session(
                session.id,
                api::ForkSession {
                    point: api::ForkPoint::Head,
                    metadata: api::UpdateSession {
                        name: Some("fork".into()),
                        ..Default::default()
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(fork.session.parent_id, Some(session.id));
        assert_eq!(fork.session.probe_id, session.probe_id);
        assert_eq!(fork.session.name, "fork");
        assert_eq!(fork.session.first_position, 5);
        assert_eq!(fork.session.next_position, 6);
        assert_eq!(fork.agents[0].0, agent.id);
        assert_eq!(fork.agents[0].1.created_position, agent.created_position);
        assert_eq!(fork.agents[0].1.first_event_id, EventId(4));
        assert_eq!(fork.agents[0].1.next_event_id, EventId(5));
        assert_eq!(fork.agents[0].1.state, api::AgentState::Interrupted);
        assert_eq!(
            sessions
                .get_agent(session.id, agent.id)
                .await
                .unwrap()
                .state,
            api::AgentState::Running
        );
        let api::StreamMessage::Committed { event, .. } = published.recv().await.unwrap() else {
            panic!("expected recovery event");
        };
        assert_eq!(event.agent_id, fork.agents[0].1.id);
        assert!(matches!(
            event.event.kind,
            EventType::StatusChange(StatusChange {
                new_status: api::AgentState::Interrupted,
                ..
            })
        ));
        assert!(
            matches!(published.recv().await.unwrap(), api::StreamMessage::SessionUpdated { session: value } if value.id == fork.session.id)
        );

        let earlier = sessions
            .fork_session(
                session.id,
                api::ForkSession {
                    point: api::ForkPoint::ThroughEvent {
                        agent_id: agent.id,
                        event_id: EventId(1),
                    },
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(earlier.session.first_position, 3);
        assert_eq!(earlier.session.next_position, 3);
        assert_eq!(earlier.agents[0].1.created_position, agent.created_position);
        assert_eq!(earlier.agents[0].1.first_event_id, EventId(2));
        let mut cursor = sessions.database.cursor().await.unwrap();
        let events = cursor
            .load_all_events(earlier.agents[0].1.id)
            .await
            .unwrap();
        cursor.rollback().await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventType::Model(_)));
        assert!(matches!(
            sessions.get_agent(session.id, fork.agents[0].1.id).await,
            Err(RuntimeError::Database(DbError::NotFound { .. }))
        ));
    }

    #[tokio::test]
    async fn session_queries_and_updates_release_database_before_manager_calls() {
        let mut fixture = Fixture::new().await;
        let session = fixture.create_session().await;
        let sessions = &fixture.sessions;

        // No manager is being polled: native read operations must not query busy.
        let loaded = tokio::time::timeout(Duration::from_secs(2), sessions.get_session(session.id))
            .await
            .expect("get_session must not await the manager")
            .unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.first_position, 1);
        assert_eq!(loaded.next_position, 1);
        let listed = tokio::time::timeout(
            Duration::from_secs(2),
            sessions.list_sessions(Default::default()),
        )
        .await
        .expect("list_sessions must not await the manager")
        .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session.id);
        assert_eq!(listed[0].first_position, 1);
        assert_eq!(listed[0].next_position, 1);

        let mut published = sessions.publication.lock().await.subscribe();
        let received = &mut fixture.received;
        let serve_manager = async {
            for busy in [true, false, true] {
                let ManagerCommand::IsBusy { session: id, reply } = received.recv().await.unwrap()
                else {
                    panic!("expected busy query");
                };
                assert_eq!(id, session.id);
                let _publication = sessions.publication.lock().await;
                sessions
                    .database
                    .cursor()
                    .await
                    .unwrap()
                    .rollback()
                    .await
                    .unwrap();
                reply.send(busy).unwrap();
            }
            let ManagerCommand::SessionUpdated { session: updated } =
                received.recv().await.unwrap()
            else {
                panic!("expected updated session notification");
            };
            assert_eq!(updated.name, "renamed");
            assert_eq!(updated.working_directory.to_str(), Some("/new"));
            let _publication = sessions.publication.lock().await;
            let mut cursor = sessions.database.cursor().await.unwrap();
            assert_eq!(
                cursor.get_session(session.id).await.unwrap().name,
                "renamed"
            );
            cursor.rollback().await.unwrap();
        };
        let query_and_update = async {
            assert!(sessions.is_busy(session.id).await.unwrap());
            assert!(!sessions.is_busy(session.id).await.unwrap());
            let updated = sessions
                .update_session(
                    session.id,
                    api::UpdateSession {
                        name: Some("renamed".into()),
                        working_directory: Some("/new".into()),
                    },
                )
                .await
                .unwrap();
            assert_eq!(updated.first_position, 1);
            assert_eq!(updated.next_position, 1);
            assert_eq!(updated.name, "renamed");
            let api::StreamMessage::SessionUpdated { session: snapshot } =
                published.recv().await.unwrap()
            else {
                panic!("expected updated session publication");
            };
            assert_eq!(snapshot.id, updated.id);
            assert_eq!(snapshot.name, updated.name);
            assert!(snapshot.busy);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(query_and_update, serve_manager);
        })
        .await
        .expect("facade must release cursors and publication gates before manager calls");
    }

    #[tokio::test]
    async fn validates_inputs_without_partial_records_or_manager_access() {
        let fixture = Fixture::new().await;
        let session = fixture.create_session().await;
        let mut invalid = create_agent_request();
        invalid.tools.push(api::ToolId::Noop);
        assert!(matches!(
            fixture.sessions.create_agent(session.id, invalid).await,
            Err(RuntimeError::Invalid(_))
        ));
        assert!(
            fixture
                .sessions
                .list_agents(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            fixture
                .sessions
                .create_session(api::CreateSession {
                    name: "missing probe".into(),
                    working_directory: "/work".into(),
                    probe_id: api::ProbeId(api::probes::Uuid::new_v4()),
                })
                .await,
            Err(RuntimeError::Database(DbError::ProbeNotFound(_)))
        ));
        drop(fixture.received);
        for delivery in [
            api::MessageDelivery::Queue,
            api::MessageDelivery::Steer,
            api::MessageDelivery::Resume,
        ] {
            let result = fixture
                .sessions
                .submit(
                    session.id,
                    AgentId(1),
                    api::AgentRequest::Message {
                        delivery,
                        content: Vec::new(),
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(RuntimeError::Invalid(message)) if message == "message content must not be empty"
            ));
        }
    }

    #[tokio::test]
    async fn forwards_request_variants_and_manager_errors() {
        let mut fixture = Fixture::new().await;
        let sessions = &fixture.sessions;
        let received = &mut fixture.received;
        let submit = async {
            let content = crate::prompts::text("test");
            assert_eq!(
                sessions
                    .submit(
                        SessionId(1),
                        AgentId(2),
                        api::AgentRequest::Message {
                            delivery: api::MessageDelivery::Queue,
                            content: content.clone()
                        }
                    )
                    .await
                    .unwrap()
                    .request_id,
                "accepted"
            );
            assert_eq!(
                sessions
                    .steer(SessionId(1), AgentId(2), content.clone())
                    .await
                    .unwrap()
                    .request_id,
                "accepted"
            );
            assert!(matches!(
                sessions.resume(SessionId(1), AgentId(2), content).await,
                Err(RuntimeError::InvalidResume)
            ));
            assert_eq!(
                sessions
                    .interrupt(SessionId(1), AgentId(2))
                    .await
                    .unwrap()
                    .request_id,
                "accepted"
            );
        };
        let respond = async {
            for delivery in [
                Some(api::MessageDelivery::Queue),
                Some(api::MessageDelivery::Steer),
                Some(api::MessageDelivery::Resume),
                None,
            ] {
                let ManagerCommand::Submit {
                    session,
                    agent,
                    request,
                    reply,
                } = received.recv().await.unwrap()
                else {
                    panic!("expected request");
                };
                assert_eq!((session, agent), (SessionId(1), AgentId(2)));
                match (delivery, request) {
                    (Some(expected), api::AgentRequest::Message { delivery, content }) => {
                        assert_eq!(delivery, expected);
                        assert_eq!(content.len(), 1);
                    }
                    (None, api::AgentRequest::Interrupt) => {}
                    _ => panic!("unexpected request variant"),
                }
                let result = if delivery == Some(api::MessageDelivery::Resume) {
                    Err(RuntimeError::InvalidResume)
                } else {
                    Ok(api::RequestAccepted {
                        request_id: "accepted".into(),
                    })
                };
                reply.send(result).unwrap();
            }
        };
        tokio::join!(submit, respond);
    }

    #[tokio::test]
    async fn reports_unavailable_for_closed_manager_or_dropped_reply() {
        let mut fixture = Fixture::new().await;
        let query = fixture.sessions.is_busy(SessionId(7));
        let drop_reply = async {
            drop(fixture.received.recv().await.unwrap());
        };
        let (result, ()) = tokio::join!(query, drop_reply);
        assert!(matches!(result, Err(RuntimeError::Unavailable)));
        drop(fixture.received);
        assert!(matches!(
            fixture.sessions.is_busy(SessionId(7)).await,
            Err(RuntimeError::Unavailable)
        ));
        assert!(matches!(
            fixture.sessions.interrupt(SessionId(7), AgentId(1)).await,
            Err(RuntimeError::Unavailable)
        ));
    }
}
