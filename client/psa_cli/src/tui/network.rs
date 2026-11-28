//! Server requests run as background tasks. Each reports back to the app task,
//! which owns all state, through [`AppEvent`]s.
use super::{app::AppEvent, draft::DraftSubmission};
use client_shared::{
    CommandId, ReconnectBackoff, SandboxAction, build_command_record, can_retry_connection,
    select_session_agent,
};
use futures_util::StreamExt;
use llm_api::providers::Model;
use psa::api::{
    AgentCommand, AgentId, AgentRequest, ClientAnswer, ClientError, ClientQueryId,
    ClientQueryResponse, CommandDelivery, CommandOutcome, CreateAgent, EventSpan, EventsQuery,
    ForkPoint, ForkSession, ListModels, MessageDelivery, PinModel, ProbeId, SessionId,
    SessionsQuery, StreamMessage, UpdateSession,
    events::{Message, MessageType},
};
use std::{future::Future, path::PathBuf};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

/// Events loaded when an agent opens; older history is not fetched yet.
const HISTORY_PAGE: u32 = 300;

#[derive(Clone)]
pub struct Network {
    client: psa::api::Client,
    events: UnboundedSender<AppEvent>,
}

impl Network {
    pub fn new(client: psa::api::Client, events: UnboundedSender<AppEvent>) -> Self {
        Self { client, events }
    }

    pub fn set_client(&mut self, client: psa::api::Client) {
        self.client = client;
    }

    pub fn events(&self) -> UnboundedSender<AppEvent> {
        self.events.clone()
    }

    /// Run `task`, reporting its error with `context`.
    fn spawn<F, Fut>(&self, context: &'static str, task: F) -> JoinHandle<()>
    where
        F: FnOnce(psa::api::Client, UnboundedSender<AppEvent>) -> Fut,
        Fut: Future<Output = Result<(), ClientError>> + Send + 'static,
    {
        let events = self.events.clone();
        let future = task(self.client.clone(), events.clone());
        tokio::spawn(async move {
            if let Err(error) = future.await {
                let _ = events.send(AppEvent::Error(format!("{context}: {error}")));
            }
        })
    }

    pub fn load_catalog(&self) {
        self.spawn(
            "Unable to load probes and models",
            |client, events| async move {
                let probes = client.list_probes().await?;
                let _ = events.send(AppEvent::Probes(probes));
                let models = client.list_models(&ListModels::default()).await?;
                let _ = events.send(AppEvent::Models(models));
                Ok(())
            },
        );
    }

    pub fn load_sessions(&self) {
        self.spawn("Unable to list sessions", |client, events| async move {
            let query = SessionsQuery {
                flat: true,
                limit: 200,
                ..Default::default()
            };
            let page = client.list_sessions(&query).await?;
            let _ = events.send(AppEvent::Sessions(page.sessions));
            Ok(())
        });
    }

    /// Open a session's root (or last interactive) agent.
    pub fn open_session(&self, session_id: SessionId) {
        self.spawn(
            "Unable to open the session",
            move |client, events| async move {
                let agents = client.list_agents(session_id).await?;
                match select_session_agent(&agents, None) {
                    Some(agent_id) => {
                        let _ = events.send(AppEvent::OpenAgent {
                            session_id,
                            agent_id,
                        });
                    }
                    None => {
                        let _ = events.send(AppEvent::Error(
                            "This session has no agents to open.".into(),
                        ));
                    }
                }
                Ok(())
            },
        );
    }

    pub fn load_agents(&self, session_id: SessionId) {
        self.spawn("Unable to list agents", move |client, events| async move {
            let agents = client.list_agents(session_id).await?;
            let _ = events.send(AppEvent::Agents { session_id, agents });
            Ok(())
        });
    }

    /// Follow one agent's events until aborted, reconnecting with backoff.
    pub fn follow_agent(
        &self,
        generation: u64,
        session_id: SessionId,
        agent_id: AgentId,
    ) -> JoinHandle<()> {
        let client = self.client.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut backoff = ReconnectBackoff::default();
            loop {
                let mut connected = false;
                let result = follow_agent_once(
                    &client,
                    &events,
                    generation,
                    session_id,
                    agent_id,
                    &mut connected,
                )
                .await;
                if connected {
                    backoff.reset();
                }
                if let Err(error) = result {
                    let retry = can_retry_connection(&error);
                    let status = if retry {
                        "reconnecting".to_owned()
                    } else {
                        format!("disconnected: {error}")
                    };
                    let _ = events.send(AppEvent::Connection { generation, status });
                    if !retry {
                        return;
                    }
                }
                if events.is_closed() {
                    return;
                }
                tokio::time::sleep(backoff.next_delay()).await;
            }
        })
    }

    pub fn send_message(&self, session_id: SessionId, agent_id: AgentId, text: String) {
        self.spawn("Message not sent", move |client, events| async move {
            let request = AgentRequest::Message {
                source: None,
                delivery: MessageDelivery::Steer,
                content: Message::text(MessageType::Human, text.clone()).content,
            };
            let result = client.submit_request(session_id, agent_id, &request).await;
            if result.is_err() {
                // Delivery may still have happened; the user checks before resending.
                let _ = events.send(AppEvent::SendFailed(text));
            }
            result.map(|_| ())
        });
    }

    pub fn interrupt(&self, session_id: SessionId, agent_id: AgentId) {
        self.spawn("Interrupt failed", move |client, events| async move {
            client.interrupt(session_id, agent_id).await?;
            let _ = events.send(AppEvent::Notice("Interrupt requested.".into()));
            Ok(())
        });
    }

    pub fn answer_query(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        query_id: ClientQueryId,
        answers: Vec<String>,
    ) {
        self.spawn("Answer not sent", move |client, _| async move {
            let answers = answers
                .into_iter()
                .map(|answer| ClientAnswer {
                    answer,
                    notes: None,
                })
                .collect();
            let response =
                ClientQueryResponse::AskQuestion(psa::api::AskQuestionResponse { answers });
            client
                .respond_to_client_query(session_id, agent_id, query_id, &response)
                .await
        });
    }

    /// Apply a session change, then record the command in the agent's transcript.
    pub fn update_session(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        command: (CommandId, String),
        update: UpdateSession,
        notice: String,
    ) {
        self.spawn(
            "Unable to record the command",
            move |client, events| async move {
                let result = client.update_session(session_id, &update).await;
                let ended = report_result(&events, result, notice, "Unable to update the session");
                if let Some(session) = ended.as_ref().ok().and_then(|s| s.clone()) {
                    let _ = events.send(AppEvent::SessionUpdated(session));
                }
                record_command(&client, session_id, agent_id, command, ended.map(|_| ())).await
            },
        );
    }

    pub fn fork_session(&self, session_id: SessionId, agent_id: AgentId, name: String) {
        self.spawn("Unable to record /fork", move |client, events| async move {
            let request = ForkSession {
                point: ForkPoint::Head,
                metadata: UpdateSession {
                    name: Some(name.clone()).filter(|name| !name.is_empty()),
                    ..Default::default()
                },
            };
            let result = client.fork_session(session_id, &request).await;
            let ended = match &result {
                Ok(_) => Ok(()),
                Err(error) => Err(format!("Unable to fork the session: {error}")),
            };
            record_command(
                &client,
                session_id,
                agent_id,
                (CommandId::Fork, name),
                ended.clone(),
            )
            .await?;

            let fork = match result {
                Ok(fork) => fork,
                Err(_) => {
                    let _ = events.send(AppEvent::Error(ended.unwrap_err()));
                    return Ok(());
                }
            };
            let agent = fork
                .agents
                .iter()
                .find(|agent| agent.forked_from == Some(agent_id))
                .or(fork.agents.first());
            if let Some(agent) = agent {
                let _ = events.send(AppEvent::Notice("Session forked.".into()));
                let _ = events.send(AppEvent::OpenAgent {
                    session_id: fork.session.id,
                    agent_id: agent.id,
                });
            }
            Ok(())
        });
    }

    pub fn refresh_configs(&self, session_id: SessionId, agent_id: AgentId) {
        self.spawn(
            "Unable to record /refresh",
            move |client, events| async move {
                let result = match client.refresh_configs(&[session_id]).await {
                    Ok(()) => client.list_agent_configs().await,
                    Err(error) => Err(error),
                };
                let ended = match result {
                    Ok(configs) => {
                        let message = format!("Reloaded {} agent configurations.", configs.len());
                        let _ = events.send(AppEvent::Notice(message));
                        Ok(())
                    }
                    Err(error) => {
                        let message = format!("Unable to refresh configurations: {error}");
                        let _ = events.send(AppEvent::Error(message.clone()));
                        Err(message)
                    }
                };
                record_command(
                    &client,
                    session_id,
                    agent_id,
                    (CommandId::Refresh, String::new()),
                    ended,
                )
                .await
            },
        );
    }

    /// Steer a server command such as `/compact` and report how it ended; the
    /// server records it in the transcript.
    pub fn run_agent_command(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        command: AgentCommand,
    ) {
        let name = command.name();
        self.spawn("Command failed", move |client, events| async move {
            let result = client
                .send_command(session_id, agent_id, CommandDelivery::Steer, command)
                .await?;
            let event = match result.outcome {
                CommandOutcome::Succeeded => AppEvent::Notice(
                    result
                        .output
                        .unwrap_or_else(|| format!("/{name} finished.")),
                ),
                CommandOutcome::Interrupted => AppEvent::Notice(format!("/{name} interrupted.")),
                CommandOutcome::Failed => {
                    AppEvent::Error(result.output.unwrap_or_else(|| format!("/{name} failed.")))
                }
            };
            let _ = events.send(event);
            Ok(())
        });
    }

    /// `/sandbox start|stop` for the session's probe, which must be a sandbox.
    pub fn change_sandbox(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        probe_id: ProbeId,
        action: SandboxAction,
    ) {
        self.spawn(
            "Unable to record /sandbox",
            move |client, events| async move {
                let sandboxes = client.list_sandboxes(None).await?;
                let ended = if !sandboxes.iter().any(|sandbox| sandbox.probe_id == probe_id) {
                    Err("This session is not running in a sandbox.".to_owned())
                } else {
                    let result = match action {
                        SandboxAction::Start => client.start_sandbox(probe_id).await,
                        SandboxAction::Stop => client.stop_sandbox(probe_id).await,
                    };
                    match result {
                        Ok(sandbox) => {
                            let message = format!(
                                "Sandbox {} {}.",
                                sandbox.name,
                                action.change().past_tense()
                            );
                            let _ = events.send(AppEvent::Notice(message));
                            Ok(())
                        }
                        Err(error) => Err(format!(
                            "Unable to {} the sandbox: {error}",
                            action.as_str()
                        )),
                    }
                };
                if let Err(message) = &ended {
                    let _ = events.send(AppEvent::Error(message.clone()));
                }
                let command = (CommandId::Sandbox, action.as_str().to_owned());
                record_command(&client, session_id, agent_id, command, ended).await
            },
        );
    }

    /// The configurations a draft in `directory` on `probe_id` can start:
    /// globals, and the directory's `.agents` ones, found and rescanned first.
    pub fn load_draft_configs(&self, generation: u64, probe_id: ProbeId, directory: PathBuf) {
        let events = self.events.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let loaded = async {
                let found = client.discover_configs(probe_id, &directory).await?;
                let configs = client.list_agent_configs().await?;
                let config = client.get_config().await?;
                Ok::<_, ClientError>((found, configs, config.default_agent_name))
            };
            let event = match loaded.await {
                Ok((found, configs, default_name)) => {
                    if !found.searched {
                        let message =
                            "The probe is offline, so its .agents configurations are not listed.";
                        let _ = events.send(AppEvent::Error(message.into()));
                    }
                    AppEvent::DraftConfigs {
                        generation,
                        agents_dir: found.group,
                        configs,
                        default_name,
                    }
                }
                Err(error) => AppEvent::DraftConfigsFailed {
                    generation,
                    message: format!("Unable to load agent configurations: {error}"),
                },
            };
            let _ = events.send(event);
        });
    }

    /// A draft's first message: create its session unless an earlier attempt
    /// did, then the root agent, then send the message and open the agent.
    pub fn submit_draft(&self, submission: DraftSubmission) {
        let events = self.events.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let DraftSubmission {
                generation,
                session,
                create,
                config_id,
                model,
                text,
            } = submission;
            let fail = |message: String| AppEvent::DraftFailed {
                generation,
                message,
                text: text.clone(),
            };

            let session = match session {
                Some(session) => session,
                None => match client.create_session(&create).await {
                    Ok(session) => {
                        let _ = events.send(AppEvent::DraftSessionCreated {
                            generation,
                            session: session.clone(),
                        });
                        session
                    }
                    Err(error) => {
                        let _ = events.send(fail(format!("Unable to create the session: {error}")));
                        return;
                    }
                },
            };
            // Record the session's own `.agents` directory before its agent uses it.
            if let Err(error) = client.refresh_configs(&[session.id]).await {
                let _ = events.send(fail(format!(
                    "Unable to load agent configurations: {error}"
                )));
                return;
            }
            let request = CreateAgent {
                name: config_id.name.clone(),
                config_id,
                subagent_parent_id: None,
                interactive: true,
                hidden: false,
                model,
            };
            let agent = match client.create_agent(session.id, &request).await {
                Ok(agent) => agent,
                Err(error) => {
                    let _ = events.send(fail(format!("Unable to create the agent: {error}")));
                    return;
                }
            };
            let _ = events.send(AppEvent::OpenAgent {
                session_id: session.id,
                agent_id: agent.id,
            });

            let request = AgentRequest::Message {
                source: None,
                delivery: MessageDelivery::Steer,
                content: Message::text(MessageType::Human, text.clone()).content,
            };
            if let Err(error) = client.submit_request(session.id, agent.id, &request).await {
                // Delivery may still have happened; the user checks before resending.
                let _ = events.send(AppEvent::SendFailed(text));
                let _ = events.send(AppEvent::Error(format!("Message not sent: {error}")));
            }
        });
    }

    /// Pin or unpin a model on the server, for `/pinned-models`.
    pub fn set_model_pinned(&self, model: Model, pinned: bool) {
        self.spawn(
            "Unable to change the pin",
            move |client, events| async move {
                let request = PinModel {
                    model: model.clone(),
                    pinned,
                };
                client.set_model_pinned(&request).await?;
                let _ = events.send(AppEvent::ModelPinned { model, pinned });
                Ok(())
            },
        );
    }

    /// Per-model usage for `/info`.
    pub fn load_model_usage(&self, session_id: SessionId, agent_id: AgentId) {
        self.spawn(
            "Unable to load usage by model",
            move |client, events| async move {
                let usage = client.get_agent_model_usage(session_id, agent_id).await?;
                let _ = events.send(AppEvent::ModelUsage { agent_id, usage });
                Ok(())
            },
        );
    }

    pub fn show_probes(&self) {
        self.spawn("Unable to list probes", |client, events| async move {
            let probes = client.list_probes().await?;
            let sandboxes = client.list_sandboxes(None).await?;
            let _ = events.send(AppEvent::Probes(probes.clone()));
            let _ = events.send(AppEvent::ProbeReport { probes, sandboxes });
            Ok(())
        });
    }
}

/// Connect once: subscribe to the whole session, so every agent's state
/// stays current for the agent bar, then load the agent's latest page. Events
/// committed while the page loads arrive in both and are kept once.
async fn follow_agent_once(
    client: &psa::api::Client,
    events: &UnboundedSender<AppEvent>,
    generation: u64,
    session_id: SessionId,
    agent_id: AgentId,
    connected: &mut bool,
) -> Result<(), ClientError> {
    let boundary = client
        .list_events(
            session_id,
            None,
            &EventsQuery {
                span: EventSpan::Latest,
                limit: 1,
                ..Default::default()
            },
        )
        .await?;
    let query = EventsQuery {
        span: EventSpan::Forward {
            after: boundary.head,
            through: None,
        },
        include_pending: true,
        ..Default::default()
    };
    let mut stream = client.stream_events(session_id, None, &query, None).await?;

    let session = client.get_session(session_id).await?;
    let agents = client.list_agents(session_id).await?;
    let page = client
        .list_events(
            session_id,
            Some(agent_id),
            &EventsQuery {
                span: EventSpan::Latest,
                include_pending: true,
                limit: HISTORY_PAGE,
                ..Default::default()
            },
        )
        .await?;
    let context_state = client
        .get_agent_context_state(session_id, agent_id)
        .await
        .ok();

    let opened = AppEvent::Opened {
        generation,
        session,
        agents,
        page,
        context_state,
    };
    if events.send(opened).is_err() {
        return Ok(());
    }
    *connected = true;

    while let Some(item) = stream.next().await {
        let message = item?.message;
        if matches!(message, StreamMessage::ResyncRequired) {
            return Err(ClientError::Stream("resync requested".into()));
        }
        if events
            .send(AppEvent::Stream {
                generation,
                message,
            })
            .is_err()
        {
            return Ok(());
        }
    }
    Err(ClientError::Stream("event stream ended".into()))
}

/// Report a request's outcome as a notice or error, returning how it ended for
/// the command record.
fn report_result<T>(
    events: &UnboundedSender<AppEvent>,
    result: Result<T, ClientError>,
    notice: String,
    context: &str,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => {
            let _ = events.send(AppEvent::Notice(notice));
            Ok(Some(value))
        }
        Err(error) => {
            let message = format!("{context}: {error}");
            let _ = events.send(AppEvent::Error(message.clone()));
            Err(message)
        }
    }
}

/// Record a finished slash command in the agent's transcript, if the client
/// saves that command.
async fn record_command(
    client: &psa::api::Client,
    session_id: SessionId,
    agent_id: AgentId,
    (id, args): (CommandId, String),
    ended: Result<(), String>,
) -> Result<(), ClientError> {
    let Some(record) = build_command_record(id, &args, ended) else {
        return Ok(());
    };
    client.record_command(session_id, agent_id, record).await?;
    Ok(())
}
