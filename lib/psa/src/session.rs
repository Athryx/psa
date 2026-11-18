//! A session and the agents in it.
use crate::{
    AgentConfigRef, AgentHandle, AgentSpec, Context, Error, EventFeed, EventPages, EventRange,
    EventUpdate, ProbeHandle, Result,
    api::{self, ConfigId, EventEnvelope, ForkPoint, SessionId, UpdateSession},
    events::Scope,
    spec::resolve_name,
};
use futures_util::StreamExt;

/// A session in a [`Context`], holding the snapshot it was last loaded with.
#[derive(Clone)]
pub struct SessionHandle {
    context: Context,
    session: api::Session,
}

impl SessionHandle {
    pub(crate) fn new(context: Context, session: api::Session) -> Self {
        Self { context, session }
    }

    /// Session ID.
    pub fn id(&self) -> SessionId {
        self.session.id
    }

    /// Display name.
    pub fn name(&self) -> &str {
        &self.session.name
    }

    /// Session metadata as last loaded.
    pub fn info(&self) -> &api::Session {
        &self.session
    }

    /// Context this session was loaded through.
    pub fn context(&self) -> &Context {
        &self.context
    }

    fn client(&self) -> &api::Client {
        self.context.client()
    }

    fn scope(&self) -> Scope {
        Scope {
            session: self.session.id,
            agent: None,
        }
    }

    /// Reload the session's metadata and busy state.
    pub async fn reload(&mut self) -> Result<()> {
        self.session = self.client().get_session(self.session.id).await?;
        Ok(())
    }

    /// Apply metadata changes; omitted fields stay unchanged.
    pub async fn update(&mut self, update: &UpdateSession) -> Result<()> {
        self.session = self
            .client()
            .update_session(self.session.id, update)
            .await?;
        Ok(())
    }

    /// Rename the session.
    pub async fn rename(&mut self, name: impl Into<String>) -> Result<()> {
        let update = UpdateSession {
            name: Some(name.into()),
            ..Default::default()
        };
        self.update(&update).await
    }

    /// Probe the session runs on.
    pub async fn probe(&self) -> Result<ProbeHandle> {
        ProbeHandle::load(self.context.clone(), self.session.probe_id).await
    }

    /// Make sure the session's probe is connected, starting its sandbox if
    /// stopped. An offline plain probe fails with [`Error::ProbeOffline`].
    pub async fn ensure_probe_up(&self) -> Result<ProbeHandle> {
        let mut probe = self.probe().await?;
        probe.ensure_up().await?;
        Ok(probe)
    }

    /// Stop the session's sandbox. Does nothing when the session runs on a
    /// plain probe.
    pub async fn stop_probe(&self) -> Result<()> {
        self.probe().await?.stop().await
    }

    /// Fork the session through `point`, keeping its metadata.
    pub async fn fork(&self, point: ForkPoint) -> Result<SessionHandle> {
        let request = api::ForkSession {
            point,
            metadata: UpdateSession::default(),
        };
        let forked = self
            .client()
            .fork_session(self.session.id, &request)
            .await?;
        Ok(SessionHandle::new(self.context.clone(), forked.session))
    }

    /// Every agent in the session, including subagents and hidden agents.
    pub async fn agents(&self) -> Result<Vec<AgentHandle>> {
        let agents = self.client().list_agents(self.session.id).await?;
        Ok(agents
            .into_iter()
            .map(|agent| AgentHandle::new(self.context.clone(), agent))
            .collect())
    }

    /// The top-level agent (not a subagent) with exactly this name.
    pub async fn agent(&self, name: &str) -> Result<Option<AgentHandle>> {
        let agents = self.agents().await?;
        Ok(agents
            .into_iter()
            .find(|agent| agent.info().subagent_parent_id.is_none() && agent.name() == name))
    }

    /// Create an agent. A layout is first registered in the context's group,
    /// replacing any config of the same name there.
    pub async fn create_agent(&self, spec: AgentSpec) -> Result<AgentHandle> {
        let config_id = self.ensure_config(&spec.config).await?;
        let request = api::CreateAgent {
            config_id,
            name: spec.name,
            subagent_parent_id: None,
            interactive: spec.interactive,
            hidden: spec.hidden,
            model: spec.model,
        };

        let agent = self
            .client()
            .create_agent(self.session.id, &request)
            .await?;
        Ok(AgentHandle::new(self.context.clone(), agent))
    }

    /// Return the top-level agent named `spec.name`, creating it from `spec`
    /// when none exists. An existing agent keeps its definition.
    pub async fn ensure_agent(&self, spec: AgentSpec) -> Result<AgentHandle> {
        if let Some(agent) = self.agent(&spec.name).await? {
            return Ok(agent);
        }
        self.create_agent(spec).await
    }

    /// Rescan the server's global configs and this session's `.agents` directory.
    pub async fn refresh_configs(&self) -> Result<()> {
        self.client().refresh_configs(&[self.session.id]).await?;
        Ok(())
    }

    /// Wait until the session has no active work.
    pub async fn wait_idle(&self) -> Result<()> {
        let mut feed = self.stream(EventRange::follow()).await?;
        if !self.client().get_session(self.session.id).await?.busy {
            return Ok(());
        }

        while let Some(update) = feed.next().await {
            if let EventUpdate::Session(session) = update?
                && !session.busy
            {
                return Ok(());
            }
        }
        Err(Error::StreamEnded)
    }

    /// Committed events in a finite range, in session-position order.
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

    /// The config an agent spec runs, registering a layout first.
    async fn ensure_config(&self, config: &AgentConfigRef) -> Result<ConfigId> {
        let group = self.context.config_group();
        match config {
            AgentConfigRef::Named(name) => {
                self.refresh_configs().await?;
                let configs: Vec<_> = self
                    .client()
                    .list_agent_configs()
                    .await?
                    .into_iter()
                    .map(|status| status.config.config_id)
                    .collect();
                resolve_name(&configs, group, name)
                    .ok_or_else(|| Error::UnknownConfig(name.clone()))
            }
            AgentConfigRef::Layout(layout) => self.context.register_layout(layout).await,
        }
    }
}
