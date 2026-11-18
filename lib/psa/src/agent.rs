//! An agent: messages, commands, runs, results, and client queries.
use crate::{
    Context, Error, EventFeed, EventPages, EventRange, EventUpdate, Result,
    api::{
        self, AgentCommand, AgentId, AgentResult, AgentState, ClientQueryId, ClientQueryRequest,
        ClientQueryResponse, CommandDelivery, CommandResult, EventEnvelope, EventSpan, EventsQuery,
        MessageDelivery, RequestAccepted, ResultRecord, SessionId, events::EventType,
    },
    events::Scope,
};
use futures_util::StreamExt;
use llm_api::api::{ContentBlock, TextBlock, Usage};
use serde::de::DeserializeOwned;

/// Message content sent to an agent. Converts from text or content blocks.
#[derive(Debug, Clone)]
pub struct Content(pub Vec<ContentBlock>);

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self(vec![ContentBlock::Text(TextBlock {
            text,
            annotations: Vec::new(),
        })])
    }
}

impl From<Vec<ContentBlock>> for Content {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self(blocks)
    }
}

/// How a run ended.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// State the agent stopped in: idle, or interrupted.
    pub state: AgentState,
    /// Newest result recorded during the run, if any.
    pub result: Option<ResultRecord>,
}

/// An agent in a session, holding the snapshot it was last loaded with.
#[derive(Clone)]
pub struct AgentHandle {
    context: Context,
    agent: api::Agent,
}

impl AgentHandle {
    pub(crate) fn new(context: Context, agent: api::Agent) -> Self {
        Self { context, agent }
    }

    /// Agent ID.
    pub fn id(&self) -> AgentId {
        self.agent.id
    }

    /// Session containing the agent.
    pub fn session_id(&self) -> SessionId {
        self.agent.session_id
    }

    /// Display name.
    pub fn name(&self) -> &str {
        &self.agent.name
    }

    /// State as last loaded.
    pub fn state(&self) -> AgentState {
        self.agent.state
    }

    /// Cumulative usage as last loaded.
    pub fn usage(&self) -> &Usage {
        &self.agent.usage
    }

    /// Agent metadata as last loaded.
    pub fn info(&self) -> &api::Agent {
        &self.agent
    }

    fn client(&self) -> &api::Client {
        self.context.client()
    }

    fn scope(&self) -> Scope {
        Scope {
            session: self.agent.session_id,
            agent: Some(self.agent.id),
        }
    }

    /// Reload the agent's metadata, state, and usage.
    pub async fn reload(&mut self) -> Result<()> {
        self.agent = self
            .client()
            .get_agent(self.agent.session_id, self.agent.id)
            .await?;
        Ok(())
    }

    /// Cumulative usage per model.
    pub async fn model_usage(&self) -> Result<Vec<api::ModelUsage>> {
        Ok(self
            .client()
            .get_agent_model_usage(self.agent.session_id, self.agent.id)
            .await?)
    }

    /// Current model and the last inference's context usage.
    pub async fn context_state(&self) -> Result<api::AgentContextState> {
        Ok(self
            .client()
            .get_agent_context_state(self.agent.session_id, self.agent.id)
            .await?)
    }

    /// Queue a message for after the current run, starting a run.
    pub async fn send(&self, content: impl Into<Content>) -> Result<RequestAccepted> {
        self.send_with(MessageDelivery::Queue, content).await
    }

    /// Deliver a message at the next boundary, including during a run.
    pub async fn steer(&self, content: impl Into<Content>) -> Result<RequestAccepted> {
        self.send_with(MessageDelivery::Steer, content).await
    }

    /// Resume an interrupted agent with a message.
    pub async fn resume(&self, content: impl Into<Content>) -> Result<RequestAccepted> {
        self.send_with(MessageDelivery::Resume, content).await
    }

    /// Send a message with an explicit delivery.
    pub async fn send_with(
        &self,
        delivery: MessageDelivery,
        content: impl Into<Content>,
    ) -> Result<RequestAccepted> {
        Ok(self
            .client()
            .send_message(
                self.agent.session_id,
                self.agent.id,
                delivery,
                content.into().0,
            )
            .await?)
    }

    /// Request interruption of the current run.
    pub async fn interrupt(&self) -> Result<RequestAccepted> {
        Ok(self
            .client()
            .interrupt(self.agent.session_id, self.agent.id)
            .await?)
    }

    /// Run a command once the agent is not running, and wait for its result.
    pub async fn command(&self, command: AgentCommand) -> Result<CommandResult> {
        self.command_with(CommandDelivery::Queue, command).await
    }

    /// Run a command with an explicit delivery, and wait for its result.
    pub async fn command_with(
        &self,
        delivery: CommandDelivery,
        command: AgentCommand,
    ) -> Result<CommandResult> {
        Ok(self
            .client()
            .send_command(self.agent.session_id, self.agent.id, delivery, command)
            .await?)
    }

    /// Queue a message and wait for the run it starts to finish.
    pub async fn run(&self, content: impl Into<Content>) -> Result<RunOutcome> {
        self.run_with(MessageDelivery::Queue, content).await
    }

    /// [`Self::run`], decoding the run's result into `T`. Fails with
    /// [`Error::NoResult`] when the run recorded none.
    pub async fn run_as<T: DeserializeOwned>(&self, content: impl Into<Content>) -> Result<T> {
        let outcome = self.run(content).await?;
        let result = outcome.result.ok_or(Error::NoResult)?;
        Ok(result.value.decode()?)
    }

    /// Send a message and wait for the run it joins or starts to finish.
    ///
    /// A queued message waits for any current run and starts its own. A steered
    /// message sent during a run joins it, so the wait ends with that run.
    pub async fn run_with(
        &self,
        delivery: MessageDelivery,
        content: impl Into<Content>,
    ) -> Result<RunOutcome> {
        let scope = self.scope();
        let start = scope.head(self.client()).await?;
        let range = EventRange::follow().starting(crate::EventStart::After(start));
        let mut feed = scope.stream(self.client(), range).await?;

        let current = self
            .client()
            .get_agent(self.agent.session_id, self.agent.id)
            .await?;
        let joins_current_run =
            delivery == MessageDelivery::Steer && current.state == AgentState::Running;
        let accepted = self.send_with(delivery, content).await?;

        let state = wait_for_run_end(&mut feed, &accepted.request_id, joins_current_run).await?;
        let result = self
            .client()
            .get_last_result(self.agent.session_id, self.agent.id)
            .await?
            .filter(|result| result.event_id.0 > start);
        Ok(RunOutcome { state, result })
    }

    /// The newest `limit` results, newest first.
    pub async fn results(&self, limit: u32) -> Result<Vec<ResultRecord<AgentResult>>> {
        Ok(self
            .client()
            .list_results(self.agent.session_id, self.agent.id, limit)
            .await?)
    }

    /// The newest result decoded into `T`, or `None` when there is none.
    pub async fn last_result_as<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let record = self
            .client()
            .get_last_result(self.agent.session_id, self.agent.id)
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        Ok(Some(record.value.decode()?))
    }

    /// Client queries the agent is waiting on.
    pub async fn pending_queries(&self) -> Result<Vec<ClientQueryRequest>> {
        let query = EventsQuery {
            span: EventSpan::Latest,
            limit: 1,
            ..Default::default()
        };
        let page = self
            .client()
            .list_events(self.agent.session_id, Some(self.agent.id), &query)
            .await?;
        Ok(page.queries)
    }

    /// Answer a client query. The first valid answer wins.
    pub async fn respond(
        &self,
        query: ClientQueryId,
        response: &ClientQueryResponse,
    ) -> Result<()> {
        Ok(self
            .client()
            .respond_to_client_query(self.agent.session_id, self.agent.id, query, response)
            .await?)
    }

    /// Committed events in a finite range, in event-ID order.
    pub async fn events(&self, range: EventRange) -> Result<Vec<EventEnvelope>> {
        self.scope().events(self.client(), range).await
    }

    /// Committed events in a finite range, fetched a page at a time.
    pub async fn event_pages(&self, range: EventRange) -> Result<EventPages> {
        self.scope().event_pages(self.client(), range).await
    }

    /// Committed events in a range, then live updates when it follows.
    pub async fn stream(&self, range: EventRange) -> Result<EventFeed> {
        self.scope().stream(self.client(), range).await
    }
}

/// Wait for a committed status change that ends a run. Runs that end before a
/// new one starts belong to earlier requests, unless the message joined the
/// current run.
async fn wait_for_run_end(
    feed: &mut EventFeed,
    request_id: &str,
    joins_current_run: bool,
) -> Result<AgentState> {
    let mut started = joins_current_run;
    while let Some(update) = feed.next().await {
        match update? {
            EventUpdate::Committed(envelope) => {
                let EventType::StatusChange(change) = &envelope.event.kind else {
                    continue;
                };
                match change.new_status {
                    AgentState::Running => started = true,
                    state if started => return Ok(state),
                    _ => {}
                }
            }
            EventUpdate::RequestFailed {
                request_id: failed,
                message,
                ..
            } if failed == request_id => return Err(Error::RequestFailed(message)),
            _ => {}
        }
    }
    Err(Error::StreamEnded)
}
