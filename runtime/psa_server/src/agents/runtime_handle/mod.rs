//! Session and agent operations shared by HTTP clients, tools, and hooks.
//!
//! Returns server-native snapshots; HTTP handlers build wire representations.
//!
//! Database transactions and publication guards are released before awaiting the
//! supervisor, which owns request queues and worker lifecycles.

use super::{
    Agent, AgentError, NewAgent,
    agent_manager::build_recovery_events,
    agent_projection::AgentProjection,
    client_queries::{ClientQueriesHandle, ClientQueryError},
    publication::Publication,
    supervisor::{RuntimeError, SupervisorClient, SupervisorCommand},
};
use crate::{components::cancellation::Cancellation, db::Database, sessions};
use psa::api::{self as api, AgentId, SessionId};

mod configurations;
mod results;

#[derive(Clone)]
pub struct RuntimeHandle {
    database: Database,
    publication: Publication,
    pub(crate) commands: SupervisorClient,
    queries: ClientQueriesHandle,
    catalog: Option<crate::models::ModelCatalog>,
    configuration: Option<crate::config_registry::ConfigRegistry>,
    probes: Option<crate::probes::ProbeRegistry>,
    /// Rescans and config writes take it exclusively; agent creation downgrades
    /// to shared after its rescan, so creations on different probes overlap.
    configuration_gate: std::sync::Arc<tokio::sync::RwLock<()>>,
    default_model_config: llm_api::providers::ModelConfig,
}

impl RuntimeHandle {
    pub(crate) fn new(
        database: Database,
        publication: Publication,
        commands: SupervisorClient,
        queries: ClientQueriesHandle,
    ) -> Self {
        Self {
            database,
            publication,
            commands,
            queries,
            catalog: None,
            configuration: None,
            probes: None,
            configuration_gate: Default::default(),
            default_model_config: crate::config::default_model_config(),
        }
    }

    pub(crate) fn with_default_model_config(
        mut self,
        default_model_config: llm_api::providers::ModelConfig,
    ) -> Self {
        self.default_model_config = default_model_config;
        self
    }

    pub(crate) fn with_catalog(mut self, catalog: Option<crate::models::ModelCatalog>) -> Self {
        self.catalog = catalog;
        self
    }

    pub(crate) fn with_configuration(
        mut self,
        configuration: Option<crate::config_registry::ConfigRegistry>,
        probes: crate::probes::ProbeRegistry,
    ) -> Self {
        self.configuration = configuration;
        self.probes = Some(probes);
        self
    }

    pub fn model_catalog(&self) -> Option<&crate::models::ModelCatalog> {
        self.catalog.as_ref()
    }

    /// A session decision model must be a decision model in the catalog; chat
    /// models cannot answer decision requests. Without a catalog nothing can be
    /// checked, so the model is accepted.
    async fn validate_decision_model(
        &self,
        model: Option<&llm_api::providers::Model>,
    ) -> Result<(), RuntimeError> {
        let (Some(model), Some(catalog)) = (model, &self.catalog) else {
            return Ok(());
        };
        let info = catalog
            .model_info(model)
            .await
            .map_err(|error| RuntimeError::Invalid(error.to_string()))?;
        match info {
            Some(info) if info.kind == api::ModelKind::Decision => Ok(()),
            Some(_) => Err(RuntimeError::Invalid(format!(
                "{} is a chat model, not a decision model",
                model.api_name
            ))),
            None => Err(RuntimeError::Invalid(format!(
                "{} is not in the model catalog",
                model.api_name
            ))),
        }
    }

    pub(crate) fn client_queries(&self) -> &ClientQueriesHandle {
        &self.queries
    }

    pub async fn respond_to_client_query(
        &self,
        session: SessionId,
        agent: AgentId,
        id: api::ClientQueryId,
        response: api::ClientQueryResponse,
    ) -> Result<(), ClientQueryError> {
        self.queries.respond(session, agent, id, response).await
    }

    pub async fn create_agent_definition(
        &self,
        request: crate::db::CreateAgentDefinition,
    ) -> Result<api::AgentDefinition, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let definition = cursor.create_agent_definition(request).await?;
        cursor.commit().await?;
        Ok(definition)
    }

    pub async fn get_agent_definition(
        &self,
        id: api::AgentDefinitionId,
    ) -> Result<api::AgentDefinition, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let definition = cursor.get_agent_definition(id).await?;
        cursor.rollback().await?;
        Ok(definition)
    }

    #[cfg(test)]
    pub(crate) async fn list_agent_definitions(
        &self,
    ) -> Result<Vec<api::AgentDefinition>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let definitions = cursor.list_agent_definitions().await?;
        cursor.rollback().await?;
        Ok(definitions)
    }

    #[cfg(test)]
    pub(crate) async fn update_agent_definition(
        &self,
        id: api::AgentDefinitionId,
        request: crate::db::UpdateAgentDefinition,
    ) -> Result<api::AgentDefinition, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let definition = cursor.update_agent_definition(id, request).await?;
        cursor.commit().await?;
        Ok(definition)
    }

    pub async fn create_session(
        &self,
        request: api::CreateSession,
    ) -> Result<sessions::Session, RuntimeError> {
        let model_config = request
            .model_config
            .unwrap_or_else(|| self.default_model_config.clone());
        self.validate_decision_model(request.decision_model.as_ref())
            .await?;
        let agents_dir = self
            .discover_session_agents_dir(request.probe_id, &request.working_directory)
            .await?;
        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let session = cursor
            .create_session(sessions::NewSession {
                name: request.name,
                working_directory: request.working_directory,
                probe_id: request.probe_id,
                project_id: request.project_id,
                model_config,
                decision_model: request.decision_model,
            })
            .await?;
        if let Some(agents_dir) = agents_dir {
            cursor
                .set_session_agents_dir(session.id, agents_dir)
                .await?;
        }
        cursor.commit().await?;

        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(false),
        });
        Ok(session)
    }

    pub async fn get_session(&self, id: SessionId) -> Result<sessions::Session, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let session = cursor.get_session(id).await?;
        cursor.rollback().await?;

        Ok(session)
    }

    /// Root sessions for `None`, otherwise the immediate forks of `parent`.
    pub async fn list_sessions(
        &self,
        parent: Option<SessionId>,
    ) -> Result<Vec<sessions::Session>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let sessions = cursor.list_sessions(parent).await?;
        cursor.rollback().await?;

        Ok(sessions)
    }

    pub async fn update_session(
        &self,
        id: SessionId,
        request: api::UpdateSession,
    ) -> Result<sessions::Session, RuntimeError> {
        let _guard = self.configuration_gate.write().await;
        self.validate_decision_model(request.decision_model.as_ref())
            .await?;
        let agents_dir = if let Some(cwd) = &request.working_directory {
            let session = self.get_session(id).await?;
            Some(
                self.discover_session_agents_dir(session.probe_id, cwd)
                    .await?,
            )
        } else {
            None
        };
        let notification = self.commands.reserve_notification().await?;

        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        let session = cursor
            .update_session(
                id,
                sessions::SessionUpdate {
                    name: request.name,
                    working_directory: request.working_directory,
                    model_config: request.model_config,
                    decision_model: request.decision_model,
                },
            )
            .await?;
        match agents_dir {
            Some(Some(agents_dir)) => cursor.set_session_agents_dir(id, agents_dir).await?,
            Some(None) => cursor.clear_session_agents_dir(id).await?,
            None => {}
        }
        cursor.commit().await?;

        notification.send(SupervisorCommand::SessionUpdated {
            session: session.clone(),
        });
        let busy = publication.is_session_busy(id);
        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(busy),
        });
        Ok(session)
    }

    pub async fn fork_session(
        &self,
        id: SessionId,
        request: api::ForkSession,
    ) -> Result<sessions::ForkedSession, RuntimeError> {
        let _guard = self.configuration_gate.write().await;
        self.validate_decision_model(request.metadata.decision_model.as_ref())
            .await?;
        let agents_dir = if let Some(cwd) = &request.metadata.working_directory {
            let source = self.get_session(id).await?;
            self.discover_session_agents_dir(source.probe_id, cwd)
                .await?
        } else {
            let mut cursor = self.database.cursor().await?;
            let known = cursor.has_session_agents_dir_discovery(id).await?;
            let agents_dir = cursor.get_session_agents_dir(id).await?;
            cursor.rollback().await?;
            known.then_some(agents_dir)
        };
        let notification = self.commands.reserve_notification().await?;
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
                        model_config: request.metadata.model_config,
                        decision_model: request.metadata.decision_model,
                    },
                },
            )
            .await?;

        let mut agents = Vec::new();
        let mut committed_messages = Vec::new();
        for agent in fork.agents {
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
            agents.push(cursor.get_agent(agent.id).await?);
        }

        let session = cursor.get_session(fork.session.id).await?;
        if let Some(agents_dir) = agents_dir {
            cursor
                .set_session_agents_dir(session.id, agents_dir)
                .await?;
        }
        cursor.commit().await?;

        notification.send(SupervisorCommand::AgentsCreated {
            agents: agents.iter().map(|agent| agent.id).collect(),
        });
        for message in committed_messages {
            publication.publish(message);
        }
        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.clone().into_api(false),
        });
        Ok(sessions::ForkedSession { session, agents })
    }

    async fn commit_agent(
        &self,
        session: SessionId,
        request: crate::agents::NewAgent,
    ) -> Result<Agent, RuntimeError> {
        let notification = self.commands.reserve_notification().await?;
        let publication = self.publication.lock().await;
        let mut cursor = self.database.cursor().await?;
        cursor.get_agent_definition(request.definition_id).await?;
        let agent = cursor
            .create_agent(
                session,
                NewAgent {
                    subagent_parent_id: request.subagent_parent_id,
                    name: request.name,
                    definition_id: request.definition_id,
                    interactive: request.interactive,
                    hidden: request.hidden,
                },
            )
            .await?;
        let session = cursor.get_session(session).await?;
        cursor.commit().await?;

        // Reserve before mutation, then enqueue synchronously after commit and
        // before publication, so cancellation/backpressure cannot lose the hook
        // or let a subscriber submit work ahead of agent creation.
        notification.send(SupervisorCommand::AgentsCreated {
            agents: vec![agent.id],
        });
        publication.publish(api::StreamMessage::AgentUpdated {
            agent: agent.clone().into(),
        });
        let busy = publication.is_session_busy(session.id);
        publication.publish(api::StreamMessage::SessionUpdated {
            session: session.into_api(busy),
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

    /// Read an agent's transcript, including inherited history, without mutation.
    pub async fn load_agent_events(
        &self,
        session: SessionId,
        agent: AgentId,
        range: crate::db::EventRange,
    ) -> Result<Vec<api::events::Event>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        cursor.require_agent_session(session, agent).await?;
        let events = cursor.load_events(agent, range).await?;
        cursor.rollback().await?;
        Ok(events)
    }

    /// Load the newest durable result from an agent's complete logical history.
    pub async fn load_last_result(
        &self,
        session: SessionId,
        agent: AgentId,
    ) -> Result<Option<api::ResultRecord>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let result = cursor.load_last_result(session, agent).await?;
        cursor.rollback().await?;
        Ok(result)
    }

    /// Load up to `limit` durable results, newest first, including visible ancestry.
    pub async fn load_last_results(
        &self,
        session: SessionId,
        agent: AgentId,
        limit: u32,
    ) -> Result<Vec<api::ResultRecord>, RuntimeError> {
        let mut cursor = self.database.cursor().await?;
        let results = cursor.load_last_results(session, agent, limit).await?;
        cursor.rollback().await?;
        Ok(results)
    }

    /// Wait until steering is queued without consuming it or changing agent state.
    /// Returns `Cancelled` on invocation cancellation, `Unavailable` on shutdown.
    pub async fn wait_for_steering(
        &self,
        session: SessionId,
        agent: AgentId,
        cancellation: &mut Cancellation,
    ) -> Result<(), RuntimeError> {
        let mut signal = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RuntimeError::Cancelled),
            result = self.commands.subscribe_steering(session, agent) => {
                result.map_err(|_| RuntimeError::Unavailable)??
            }
        };
        loop {
            if cancellation.is_cancelled() {
                return Err(RuntimeError::Cancelled);
            }
            match *signal.borrow_and_update() {
                Some(true) => return Ok(()),
                None => return Err(RuntimeError::Unavailable),
                Some(false) => {}
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(RuntimeError::Cancelled),
                result = signal.changed() => {
                    result.map_err(|_| RuntimeError::Unavailable)?;
                }
            }
        }
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

    /// Run a command at its delivery point and wait for its result. A command
    /// discarded before it finishes, such as by shutdown, is `Cancelled`.
    pub async fn run_command(
        &self,
        session: SessionId,
        agent: AgentId,
        request: api::RunCommand,
    ) -> Result<api::CommandResult, RuntimeError> {
        let result = self
            .commands
            .run_command(session, agent, request)
            .await
            .map_err(|_| RuntimeError::Unavailable)??;
        result.await.map_err(|_| RuntimeError::Cancelled)
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
                source: None,
            },
        )
        .await
    }

    /// Steer a message from `sender` to `recipient`, returning the link the
    /// sender records on its tool result.
    pub async fn send_subagent_message(
        &self,
        session: SessionId,
        recipient: &Agent,
        sender: &Agent,
        content: Vec<llm_api::api::ContentBlock>,
    ) -> Result<api::events::AgentMessageLink, RuntimeError> {
        let accepted = self
            .submit(
                session,
                recipient.id,
                api::AgentRequest::Message {
                    delivery: api::MessageDelivery::Steer,
                    content,
                    source: Some(sender.id),
                },
            )
            .await?;
        Ok(api::events::AgentMessageLink {
            request_id: accepted.request_id,
            sender: sender.id,
            sender_name: sender.name.clone(),
            recipient: recipient.id,
            recipient_name: recipient.name.clone(),
        })
    }

    /// Background hooks use this conditional submission rather than a racy state check.
    pub(crate) async fn steer_if_not_interrupted(
        &self,
        session: SessionId,
        agent: AgentId,
        request: api::AgentRequest,
    ) -> Result<api::RequestAccepted, RuntimeError> {
        self.commands
            .submit_if_not_interrupted(session, agent, request)
            .await
            .map_err(|_| RuntimeError::Unavailable)?
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
                source: None,
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
    use crate::{agents::supervisor::SupervisorCommand, db::DbError};
    use psa::api::events::{EventId, EventType, StatusChange};
    use std::time::Duration;

    pub(super) struct Fixture {
        _directory: tempfile::TempDir,
        pub(super) sessions: RuntimeHandle,
        _queries: crate::agents::client_queries::ClientQueries,
        received: actors::Mailbox<SupervisorCommand>,
    }

    impl Fixture {
        pub(super) async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let database = Database::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            crate::test_support::register_probe(&database).await;
            let (commands, received) = actors::channel(256);
            let publication = Publication::default();
            let queries = crate::agents::client_queries::ClientQueries::start(publication.clone());
            Self {
                _directory: directory,
                sessions: RuntimeHandle::new(
                    database,
                    publication,
                    SupervisorClient::new(commands),
                    queries.handle(),
                ),
                _queries: queries,
                received,
            }
        }

        pub(super) async fn create_session(&self) -> sessions::Session {
            self.sessions
                .create_session(api::CreateSession {
                    decision_model: None,
                    name: "test".into(),
                    working_directory: "/work".into(),
                    probe_id: api::ProbeId(api::probes::Uuid::nil()),
                    project_id: None,
                    model_config: Some(crate::test_support::model_config()),
                })
                .await
                .unwrap()
        }
    }

    pub(super) fn create_agent_request() -> crate::agents::NewAgent {
        crate::agents::NewAgent {
            subagent_parent_id: None,
            interactive: false,
            hidden: false,
            name: "agent".into(),
            definition_id: api::AgentDefinitionId(1),
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
        assert_eq!(agent.next_event_id, EventId(1));
        assert_eq!(agent.definition_id, api::AgentDefinitionId(1));
        assert_eq!(
            sessions
                .get_session(session.id)
                .await
                .unwrap()
                .next_position,
            2
        );
        let listed: Vec<crate::agents::Agent> = sessions.list_agents(session.id).await.unwrap();
        assert_eq!(listed[0].id, agent.id);
        assert_eq!(listed[0].created_position, agent.created_position);
        assert_eq!(listed[0].first_event_id, agent.first_event_id);
        let loaded: crate::agents::Agent = sessions.get_agent(session.id, agent.id).await.unwrap();
        assert_eq!(loaded.name, "agent");
        assert_eq!(loaded.created_position, agent.created_position);
        assert_eq!(loaded.first_event_id, agent.first_event_id);
        assert!(
            matches!(published.recv().await.unwrap(), api::StreamMessage::AgentUpdated { agent: value } if value.id == agent.id)
        );
        assert!(
            matches!(published.recv().await.unwrap(), api::StreamMessage::SessionUpdated { session: value } if value.id == session.id && value.updated_at == agent.updated_at)
        );

        assert!(matches!(
            published.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        // The fake manager does not run startup hooks. Seed an explicit transcript
        // so both head recovery and an earlier model-only fork can be tested.
        let mut cursor = sessions.database.cursor().await.unwrap();
        cursor
            .add_events(
                agent.id,
                &[
                    crate::db::NewEvent::new(EventType::Model(crate::test_support::model_config())),
                    crate::db::NewEvent::new(EventType::NewContext),
                    crate::db::NewEvent::new(EventType::StatusChange(StatusChange {
                        new_status: api::AgentState::Running,
                        reason: None,
                    })),
                ],
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
        assert_eq!(fork.agents[0].parent_id, Some(agent.id));
        assert_eq!(fork.agents[0].created_position, agent.created_position);
        assert_eq!(fork.agents[0].first_event_id, EventId(4));
        assert_eq!(fork.agents[0].next_event_id, EventId(5));
        assert_eq!(fork.agents[0].state, api::AgentState::Interrupted);
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
        assert_eq!(event.agent_id, fork.agents[0].id);
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
        assert_eq!(earlier.agents[0].created_position, agent.created_position);
        assert_eq!(earlier.agents[0].first_event_id, EventId(2));
        let mut cursor = sessions.database.cursor().await.unwrap();
        let events = cursor.load_all_events(earlier.agents[0].id).await.unwrap();
        cursor.rollback().await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventType::Model(_)));
        assert!(matches!(
            sessions.get_agent(session.id, fork.agents[0].id).await,
            Err(RuntimeError::Database(DbError::NotFound { .. }))
        ));
    }

    #[tokio::test]
    async fn definition_updates_create_immutable_versions_without_manager_access() {
        let fixture = Fixture::new().await;
        let sessions = &fixture.sessions;
        let initial = sessions
            .create_agent_definition(crate::db::CreateAgentDefinition {
                name: "custom".into(),
                description: "first version".into(),
                selectable: true,
                model: crate::test_support::model_config().into(),
                bundles: vec![api::BundleConfig::ExtraSystemPrompt {
                    prompt: "original instructions".into(),
                }],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(initial.version, 1);
        assert_eq!(initial.previous_version_id, None);
        let updated = sessions
            .update_agent_definition(
                initial.id,
                crate::db::UpdateAgentDefinition {
                    name: "replacement".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_ne!(updated.id, initial.id);
        assert_eq!(updated.previous_version_id, Some(initial.id));
        assert_eq!(updated.version, 2);
        assert_eq!(updated.name, "replacement");
        assert!(updated.description.is_empty());
        assert!(!updated.selectable);
        assert_eq!(updated.model, api::AgentModel::Global);
        assert!(updated.bundles.is_empty());
        let original = sessions.get_agent_definition(initial.id).await.unwrap();
        assert_eq!(original.name, "custom");
        assert_eq!(original.description, "first version");
        assert!(original.selectable);
        assert_eq!(original.model, crate::test_support::model_config().into());
        assert_eq!(
            original.bundles,
            vec![api::BundleConfig::ExtraSystemPrompt {
                prompt: "original instructions".into()
            }]
        );
        let definitions = sessions.list_agent_definitions().await.unwrap();
        assert!(
            definitions
                .iter()
                .any(|definition| definition.id == initial.id)
        );
        assert!(
            definitions
                .iter()
                .any(|definition| definition.id == updated.id)
        );
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
        sessions
            .publication
            .update_session_busy(&sessions.database, session.id, true)
            .await
            .unwrap();
        let mut published = sessions.publication.lock().await.subscribe();
        let received = &mut fixture.received;
        let serve_manager = async {
            // Only the two explicit reads below consult the manager; the update uses the projection.
            for busy in [true, false] {
                let SupervisorCommand::IsBusy { session: id, reply } =
                    received.recv().await.unwrap()
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
            let SupervisorCommand::SessionUpdated { session: updated } =
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
                        ..Default::default()
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
    async fn metadata_updates_publish_the_busy_projection_under_the_same_gate() {
        let fixture = Fixture::new().await;
        let session = fixture.create_session().await;
        let sessions = &fixture.sessions;
        let mut published = sessions.publication.lock().await.subscribe();
        // Queue the busy transition first, then metadata, while both wait on the same gate.
        let publication = sessions.publication.lock().await;
        assert!(!publication.is_session_busy(session.id));
        let start = sessions
            .publication
            .update_session_busy(&sessions.database, session.id, true);
        tokio::pin!(start);
        assert!(futures_util::poll!(&mut start).is_pending());
        let update = sessions.update_session(
            session.id,
            api::UpdateSession {
                name: Some("renamed while busy".into()),
                ..Default::default()
            },
        );
        tokio::pin!(update);
        assert!(futures_util::poll!(&mut update).is_pending());
        drop(publication);
        let (started, updated) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(&mut start, &mut update)
        })
        .await
        .expect("metadata update must not query the unpolled manager");
        started.unwrap();
        let updated = updated.unwrap();
        assert_eq!(updated.name, "renamed while busy");
        let api::StreamMessage::SessionUpdated { session: started } =
            published.recv().await.unwrap()
        else {
            panic!("expected worker-start session snapshot");
        };
        assert!(started.busy);
        let api::StreamMessage::SessionUpdated { session: metadata } =
            published.recv().await.unwrap()
        else {
            panic!("expected metadata session snapshot");
        };
        assert!(
            metadata.busy,
            "metadata must not overwrite the preceding busy=true snapshot"
        );
        assert_eq!(metadata.name, updated.name);

        sessions
            .publication
            .update_session_busy(&sessions.database, session.id, false)
            .await
            .unwrap();
        let api::StreamMessage::SessionUpdated { session: stopped } =
            published.recv().await.unwrap()
        else {
            panic!("expected worker-stop session snapshot");
        };
        assert!(!stopped.busy);
        assert_eq!(
            stopped.name, updated.name,
            "busy transitions must include the latest metadata"
        );
        assert!(
            !sessions
                .publication
                .lock()
                .await
                .is_session_busy(session.id)
        );
    }

    #[tokio::test]
    async fn validates_inputs_without_partial_records_or_manager_access() {
        let fixture = Fixture::new().await;
        let session = fixture.create_session().await;
        let mut invalid = create_agent_request();
        invalid.definition_id = api::AgentDefinitionId(9999);
        assert!(matches!(
            fixture.sessions.create_agent(session.id, invalid).await,
            Err(RuntimeError::Database(DbError::NotFound { .. }))
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
                    decision_model: None,
                    name: "missing probe".into(),
                    working_directory: "/work".into(),
                    probe_id: api::ProbeId(api::probes::Uuid::new_v4()),
                    project_id: None,
                    model_config: Some(crate::test_support::model_config()),
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
                        source: None,
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
                            source: None,
                            delivery: api::MessageDelivery::Queue,
                            content: content.clone()
                        }
                    )
                    .await
                    .unwrap()
                    .request_id,
                psa::api::RequestId::from("accepted")
            );
            assert_eq!(
                sessions
                    .steer(SessionId(1), AgentId(2), content.clone())
                    .await
                    .unwrap()
                    .request_id,
                psa::api::RequestId::from("accepted")
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
                psa::api::RequestId::from("accepted")
            );
        };
        let respond = async {
            for delivery in [
                Some(api::MessageDelivery::Queue),
                Some(api::MessageDelivery::Steer),
                Some(api::MessageDelivery::Resume),
                None,
            ] {
                let SupervisorCommand::Submit {
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
                    (
                        Some(expected),
                        api::AgentRequest::Message {
                            delivery, content, ..
                        },
                    ) => {
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
