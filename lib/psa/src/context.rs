//! Connection context: the server client, the project, and the probes its
//! sessions may run on.
use crate::{
    AgentLayout, Error, ProbeHandle, ProbeSelector, Result, SessionHandle, SessionProbe,
    SessionSpec, Skill,
    api::{
        self, ArchiveCompression, ConfigGroup, ConfigId, NewSkill, ProbeId, ProjectId,
        SandboxState, SessionId,
    },
    spec::{InlineDefinitions, RegisteredNames},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Settings for a [`Context`], resolved against the server by [`Self::build`].
pub struct ContextBuilder {
    client: api::Client,
    project: String,
    probe: Option<ProbeSelector>,
    working_directory: Option<PathBuf>,
}

impl ContextBuilder {
    /// Restrict the context to this probe and the sandboxes it manages. It is
    /// also the default probe for new sessions and the manager of new sandboxes.
    /// Without a restriction every probe may be used, and the host probe is the
    /// default.
    pub fn probe(mut self, selector: ProbeSelector) -> Self {
        self.probe = Some(selector);
        self
    }

    /// Default working directory for new sessions. Defaults to the session
    /// probe's home directory.
    pub fn working_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(path.into());
        self
    }

    /// Validate the project and resolve the probe restriction.
    pub async fn build(self) -> Result<Context> {
        let project = ProjectId(self.project);
        project.validate().map_err(Error::InvalidProject)?;

        let mut context = Context {
            inner: Arc::new(ContextInner {
                client: self.client,
                config_group: ConfigGroup::Custom(project.0.clone()),
                project,
                restriction: None,
                working_directory: self.working_directory,
            }),
        };
        if let Some(selector) = self.probe {
            // The probe handle (a context clone) is dropped with this statement.
            let id = context.find_probe(selector).await?.id();
            Arc::get_mut(&mut context.inner)
                .expect("context is not shared yet")
                .restriction = Some(id);
        }
        Ok(context)
    }
}

/// Entry point of the high-level API: a server connection scoped to a
/// project, whose name is also the config group agent layouts are registered
/// in. Cheap to clone.
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

struct ContextInner {
    client: api::Client,
    project: ProjectId,
    config_group: ConfigGroup,
    restriction: Option<ProbeId>,
    working_directory: Option<PathBuf>,
}

impl Context {
    /// Start building a context for a project on an HTTP(S) server.
    pub fn connect(
        base: &str,
        client_key: &str,
        project: impl Into<String>,
    ) -> Result<ContextBuilder> {
        Ok(Self::builder(api::Client::new(base, client_key)?, project))
    }

    /// Start building a context for a project around an existing wire client.
    pub fn builder(client: api::Client, project: impl Into<String>) -> ContextBuilder {
        ContextBuilder {
            client,
            project: project.into(),
            probe: None,
            working_directory: None,
        }
    }

    /// Wire client, for operations this layer does not cover.
    pub fn client(&self) -> &api::Client {
        &self.inner.client
    }

    /// Project the context's sessions belong to.
    pub fn project(&self) -> &ProjectId {
        &self.inner.project
    }

    /// `ConfigGroup::Custom(<project>)`: where agent layouts are registered,
    /// and searched first for named configs.
    pub fn config_group(&self) -> &ConfigGroup {
        &self.inner.config_group
    }

    /// Probe the context is restricted to, together with its sandboxes.
    pub fn probe_restriction(&self) -> Option<ProbeId> {
        self.inner.restriction
    }

    /// Default working directory for new sessions.
    pub fn working_directory(&self) -> Option<&Path> {
        self.inner.working_directory.as_deref()
    }

    /// Every probe and sandbox the context may use. Deleted sandboxes are left out.
    pub async fn probes(&self) -> Result<Vec<ProbeHandle>> {
        let probes = self.client().list_probes().await?;
        let sandboxes = self.client().list_sandboxes(self.inner.restriction).await?;

        let mut handles: Vec<_> = probes
            .iter()
            .filter(|probe| {
                self.inner
                    .restriction
                    .is_none_or(|id| id == probe.metadata.id)
            })
            .filter(|probe| {
                !sandboxes
                    .iter()
                    .any(|sandbox| sandbox.probe_id == probe.metadata.id)
            })
            .map(|probe| {
                ProbeHandle::new(self.clone(), probe.metadata.id, Some(probe.clone()), None)
            })
            .collect();
        handles.extend(
            sandboxes
                .into_iter()
                .filter(|sandbox| sandbox.state != SandboxState::Deleted)
                .map(|sandbox| {
                    let probe = probes
                        .iter()
                        .find(|probe| probe.metadata.id == sandbox.probe_id)
                        .cloned();
                    ProbeHandle::new(self.clone(), sandbox.probe_id, probe, Some(sandbox))
                }),
        );
        Ok(handles)
    }

    /// The one usable probe matching `selector`.
    pub async fn find_probe(&self, selector: ProbeSelector) -> Result<ProbeHandle> {
        let mut matching = self
            .probes()
            .await?
            .into_iter()
            .filter(|probe| selector.matches(probe));
        let Some(probe) = matching.next() else {
            return Err(Error::ProbeNotFound(selector));
        };
        if matching.next().is_some() {
            return Err(Error::AmbiguousProbe(selector));
        }
        Ok(probe)
    }

    /// Load a usable probe or sandbox by ID.
    pub async fn probe(&self, id: ProbeId) -> Result<ProbeHandle> {
        let probe = ProbeHandle::load(self.clone(), id).await?;
        if !self.allows(&probe) {
            return Err(Error::ProbeNotAllowed(id));
        }
        Ok(probe)
    }

    /// The probe new sessions use by default: the restriction, else the host probe.
    pub async fn default_probe(&self) -> Result<ProbeHandle> {
        match self.inner.restriction {
            Some(id) => self.probe(id).await,
            None => self.find_probe(ProbeSelector::Host).await,
        }
    }

    /// Create a sandbox on the default probe, without waiting for it to connect.
    pub async fn create_sandbox(&self, request: &api::CreateSandbox) -> Result<ProbeHandle> {
        let manager = self.default_probe().await?;
        let sandbox = self.client().create_sandbox(manager.id(), request).await?;
        Ok(ProbeHandle::new(
            self.clone(),
            sandbox.probe_id,
            None,
            Some(sandbox),
        ))
    }

    /// Every session in the project on a usable probe, including forks,
    /// newest update first.
    pub async fn sessions(&self) -> Result<Vec<SessionHandle>> {
        self.query_sessions(String::new()).await
    }

    /// The most recently updated session in the project with exactly this name.
    pub async fn find_session(&self, name: &str) -> Result<Option<SessionHandle>> {
        let sessions = self.query_sessions(name.to_owned()).await?;
        Ok(sessions.into_iter().find(|session| session.name() == name))
    }

    /// Fetch a session by ID, regardless of its project.
    pub async fn session(&self, id: SessionId) -> Result<SessionHandle> {
        let session = self.client().get_session(id).await?;
        Ok(SessionHandle::new(self.clone(), session))
    }

    /// Create a session in the project. Its probe is brought up first: a
    /// stopped sandbox is started, and an offline plain probe is an error.
    pub async fn create_session(&self, spec: SessionSpec) -> Result<SessionHandle> {
        let mut probe = match spec.probe {
            SessionProbe::Default => self.default_probe().await?,
            SessionProbe::Select(selector) => self.find_probe(selector).await?,
            SessionProbe::NewSandbox(request) => self.create_sandbox(&request).await?,
        };
        probe.ensure_up().await?;

        let working_directory = spec
            .working_directory
            .or_else(|| self.inner.working_directory.clone())
            .or_else(|| {
                let home = probe.info()?.metadata.home_dir.as_ref()?;
                Some(PathBuf::from(home))
            })
            .ok_or(Error::MissingWorkingDirectory)?;
        let request = api::CreateSession {
            name: spec.name,
            working_directory,
            probe_id: probe.id(),
            project_id: Some(self.inner.project.clone()),
            model_config: spec.model,
            decision_model: spec.decision_model,
        };

        let session = self.client().create_session(&request).await?;
        Ok(SessionHandle::new(self.clone(), session))
    }

    /// Return the newest session named `spec.name`, creating it from `spec`
    /// when none exists. An existing session is returned unchanged; use
    /// [`SessionHandle::ensure_probe_up`] before running it.
    pub async fn ensure_session(&self, spec: SessionSpec) -> Result<SessionHandle> {
        if let Some(session) = self.find_session(&spec.name).await? {
            return Ok(session);
        }
        self.create_session(spec).await
    }

    /// Upload `skill` into the context's group, replacing a skill of the same
    /// name. Nothing is uploaded when the registered archive is identical.
    pub async fn register_skill(&self, skill: &Skill) -> Result<ConfigId> {
        let registered = self.client().list_skills().await?;
        self.upload_skill(skill, &registered).await
    }

    /// Register `layout` in the context's group, with every layout and skill it
    /// includes inline, however deeply nested. Each replaces the config or
    /// skill of the same name; unchanged skills are not uploaded again. Named
    /// inclusions resolve against the inline definitions and the registered
    /// configs, in the context's group first, then the global group. An inline
    /// name defined twice in the tree must have equal definitions.
    pub async fn register_layout(&self, layout: &AgentLayout) -> Result<ConfigId> {
        let definitions = InlineDefinitions::collect(layout)?;

        let mut registered = RegisteredNames::default();
        if definitions.has_named {
            self.client().refresh_configs(&[]).await?;
            let configs = self.client().list_agent_configs().await?;
            registered.configs = configs
                .into_iter()
                .map(|status| status.config.config_id)
                .collect();
        }
        let registered_skills = if definitions.has_named || !definitions.skills.is_empty() {
            self.client().list_skills().await?
        } else {
            Vec::new()
        };
        registered.skills = registered_skills
            .iter()
            .map(|skill| skill.config_id.clone())
            .collect();
        // Inline definitions land in the own group, where names resolve first.
        registered.configs.extend(
            definitions
                .layouts
                .iter()
                .map(|layout| self.config_id(&layout.name)),
        );
        registered.skills.extend(
            definitions
                .skills
                .iter()
                .map(|skill| self.config_id(skill.name())),
        );

        // Resolve every name before uploading anything.
        let inputs = definitions
            .layouts
            .iter()
            .map(|layout| layout.build_config_input(self.config_group(), &registered))
            .collect::<Result<Vec<_>>>()?;

        for skill in &definitions.skills {
            self.upload_skill(skill, &registered_skills).await?;
        }
        self.client().upsert_agent_configs(&inputs).await?;
        Ok(self.config_id(&layout.name))
    }

    fn config_id(&self, name: &str) -> ConfigId {
        ConfigId {
            config_group: self.config_group().clone(),
            name: name.to_owned(),
        }
    }

    async fn upload_skill(&self, skill: &Skill, registered: &[api::Skill]) -> Result<ConfigId> {
        let config_id = self.config_id(skill.name());
        let unchanged = registered.iter().any(|existing| {
            existing.config_id == config_id
                && existing.archive_sha256.as_deref() == Some(skill.archive_sha256())
        });
        if unchanged {
            return Ok(config_id);
        }

        let request = NewSkill {
            config_id: config_id.clone(),
            tar_data: skill.archive().to_vec(),
            compression: ArchiveCompression::Zstd,
        };
        self.client().upload_skill(&request).await?;
        Ok(config_id)
    }

    fn allows(&self, probe: &ProbeHandle) -> bool {
        let Some(restriction) = self.inner.restriction else {
            return true;
        };
        probe.id() == restriction
            || probe
                .sandbox()
                .is_some_and(|sandbox| sandbox.manager_id == restriction)
    }

    async fn query_sessions(&self, name_contains: String) -> Result<Vec<SessionHandle>> {
        // Sessions on deleted sandboxes still belong to the restricted probe.
        let probe_ids = match self.inner.restriction {
            Some(restriction) => {
                let sandboxes = self.client().list_sandboxes(Some(restriction)).await?;
                std::iter::once(restriction)
                    .chain(sandboxes.iter().map(|sandbox| sandbox.probe_id))
                    .collect()
            }
            None => Vec::new(),
        };
        let mut query = api::SessionsQuery {
            flat: true,
            filter: api::SessionFilter {
                name_contains,
                probe_ids,
                project_ids: vec![Some(self.inner.project.clone())],
            },
            ..Default::default()
        };

        let mut sessions = Vec::new();
        loop {
            let page = self.client().list_sessions(&query).await?;
            sessions.extend(
                page.sessions
                    .into_iter()
                    .map(|session| SessionHandle::new(self.clone(), session)),
            );
            let Some(before) = page.next_before else {
                return Ok(sessions);
            };
            query.before = Some(before);
        }
    }
}
