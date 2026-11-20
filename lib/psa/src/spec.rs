//! Descriptions of sessions and agents, translated into wire requests.
use crate::{
    Error, ProbeSelector, Result, Skill, SkillRef,
    api::{
        AgentConfigInput, BundleConfig, ConfigGroup, ConfigId, CreateSandbox, IncludedItems,
        ModelSelection,
    },
};
use llm_api::providers::{Model, ModelConfig};
use std::path::PathBuf;

/// Settings for a new session.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Display name; [`crate::Context::ensure_session`] finds sessions by it.
    pub name: String,
    /// Probe the session runs on.
    pub probe: SessionProbe,
    /// Working directory on the context's probe; defaults to the context's.
    pub working_directory: Option<PathBuf>,
    /// Session model; defaults to the server's session defaults.
    pub model: Option<ModelConfig>,
    /// Session decision model, if any.
    pub decision_model: Option<Model>,
}

impl SessionSpec {
    /// A session with this name and default settings.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            probe: SessionProbe::Default,
            working_directory: None,
            model: None,
            decision_model: None,
        }
    }

    /// Run on the usable probe or sandbox matching `selector`.
    pub fn probe(mut self, selector: ProbeSelector) -> Self {
        self.probe = SessionProbe::Select(selector);
        self
    }

    /// Run in a new sandbox created on the context's default probe.
    pub fn new_sandbox(mut self, request: CreateSandbox) -> Self {
        self.probe = SessionProbe::NewSandbox(request);
        self
    }

    /// Run in this directory instead of the context's default.
    pub fn working_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(path.into());
        self
    }

    /// Use this session model.
    pub fn model(mut self, model: ModelConfig) -> Self {
        self.model = Some(model);
        self
    }

    /// Use this session decision model.
    pub fn decision_model(mut self, model: Model) -> Self {
        self.decision_model = Some(model);
        self
    }
}

/// Which probe a new session runs on.
#[derive(Debug, Clone, Default)]
pub enum SessionProbe {
    /// The context's probe restriction, else the host probe.
    #[default]
    Default,
    /// The usable probe or sandbox matching this selector.
    Select(ProbeSelector),
    /// A new sandbox on the context's default probe.
    NewSandbox(CreateSandbox),
}

/// An agent to create in a session.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Agent display name; [`crate::SessionHandle::ensure_agent`] finds agents by it.
    pub name: String,
    /// What the agent runs.
    pub config: AgentConfigRef,
    /// Model choice; must be allowed by the config. `None` uses its default.
    pub model: Option<ModelSelection>,
    /// Whether the agent is marked interactive for its hooks and tools.
    pub interactive: bool,
    /// Whether clients should hide the agent.
    pub hidden: bool,
}

impl AgentSpec {
    /// An agent running `config`: a config name (`"reviewer"`) or an [`AgentLayout`].
    pub fn new(name: impl Into<String>, config: impl Into<AgentConfigRef>) -> Self {
        Self {
            name: name.into(),
            config: config.into(),
            model: None,
            interactive: false,
            hidden: false,
        }
    }

    /// Choose this model instead of the config's default.
    pub fn model(mut self, model: ModelSelection) -> Self {
        self.model = Some(model);
        self
    }

    /// Mark the agent interactive.
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    /// Mark the agent hidden from clients.
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

/// The configuration an [`AgentSpec`] runs, or a subagent an [`AgentLayout`] includes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentConfigRef {
    /// A registered config, looked up in the context's group, then the global group.
    Named(String),
    /// A layout registered in the context's group before the agent is created.
    Layout(AgentLayout),
}

impl AgentConfigRef {
    /// Config name this refers to.
    pub fn name(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Layout(layout) => &layout.name,
        }
    }
}

impl From<AgentLayout> for AgentConfigRef {
    fn from(layout: AgentLayout) -> Self {
        Self::Layout(layout)
    }
}

impl From<&str> for AgentConfigRef {
    fn from(name: &str) -> Self {
        Self::Named(name.to_owned())
    }
}

impl From<String> for AgentConfigRef {
    fn from(name: String) -> Self {
        Self::Named(name)
    }
}

/// An agent config described in code. Registering it creates or replaces the
/// config of the same name in the context's group, along with the layouts and
/// skills it includes inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLayout {
    /// Config name within the context's group.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Offer this config in clients' agent pickers.
    pub selectable: bool,
    /// Allowed models, in order; the first is the default.
    pub models: Vec<ModelSelection>,
    /// Bundles to install; `None` uses the server's defaults.
    pub bundles: Option<Vec<BundleConfig>>,
    /// Agent configs this agent may start as subagents.
    pub subagents: Inclusions<AgentConfigRef>,
    /// Skills available to this agent.
    pub skills: Inclusions<SkillRef>,
}

impl AgentLayout {
    /// A layout following the session's model, with the server's default
    /// bundles, no subagents, and no skills.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            selectable: false,
            models: vec![ModelSelection::Global],
            bundles: None,
            subagents: Inclusions::default(),
            skills: Inclusions::default(),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Offer this config in clients' agent pickers.
    pub fn selectable(mut self) -> Self {
        self.selectable = true;
        self
    }

    /// Replace the allowed models; the first is the default.
    pub fn models(mut self, models: Vec<ModelSelection>) -> Self {
        self.models = models;
        self
    }

    /// Install exactly these bundles instead of the server's defaults.
    pub fn bundles(mut self, bundles: Vec<BundleConfig>) -> Self {
        self.bundles = Some(bundles);
        self
    }

    /// Set the subagents this agent may start.
    pub fn subagents(mut self, subagents: Inclusions<AgentConfigRef>) -> Self {
        self.subagents = subagents;
        self
    }

    /// Set the skills available to this agent.
    pub fn skills(mut self, skills: Inclusions<SkillRef>) -> Self {
        self.skills = skills;
        self
    }

    /// Build the upsert input for this layout in `group`, resolving named
    /// inclusions against the registered names.
    pub(crate) fn build_config_input(
        &self,
        group: &ConfigGroup,
        registered: &RegisteredNames,
    ) -> Result<AgentConfigInput> {
        let config_id = ConfigId {
            config_group: group.clone(),
            name: self.name.clone(),
        };
        let mut input = AgentConfigInput::new(config_id, self.models.clone());
        input.description = self.description.clone();
        input.selectable = self.selectable;
        input.bundles = self.bundles.clone();
        input.subagents = self.subagents.build_included_items(
            group,
            &registered.configs,
            AgentConfigRef::name,
            Error::UnknownConfig,
        )?;
        input.skills = self.skills.build_included_items(
            group,
            &registered.skills,
            SkillRef::name,
            Error::UnknownSkill,
        )?;
        Ok(input)
    }
}

/// A selection of agent configs ([`AgentConfigRef`]) or skills ([`SkillRef`])
/// by group and by item. Named items resolve in the context's group first, then
/// the global group; inline items are registered in the context's group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inclusions<R> {
    /// Include every item of the global group.
    pub global: bool,
    /// Include every item of the context's own group.
    pub group: bool,
    /// Include these items, by name or inline.
    pub items: Vec<R>,
}

impl<R> Default for Inclusions<R> {
    fn default() -> Self {
        Self {
            global: false,
            group: false,
            items: Vec::new(),
        }
    }
}

impl<R> Inclusions<R> {
    /// Include every item of the global group.
    pub fn include_global(mut self) -> Self {
        self.global = true;
        self
    }

    /// Include every item of the context's own group.
    pub fn include_group(mut self) -> Self {
        self.group = true;
        self
    }

    /// Include one item: a name, or an inline layout or skill.
    pub fn include(mut self, item: impl Into<R>) -> Self {
        self.items.push(item.into());
        self
    }

    fn build_included_items(
        &self,
        group: &ConfigGroup,
        registered: &[ConfigId],
        name_of: fn(&R) -> &str,
        unknown: fn(String) -> Error,
    ) -> Result<IncludedItems> {
        // Later groups override earlier ones, so the own group follows Global.
        let mut groups = Vec::new();
        if self.global {
            groups.push(ConfigGroup::Global);
        }
        if self.group {
            groups.push(group.clone());
        }

        let items = self
            .items
            .iter()
            .map(|item| {
                let name = name_of(item);
                resolve_name(registered, group, name).ok_or_else(|| unknown(name.to_owned()))
            })
            .collect::<Result<_>>()?;
        Ok(IncludedItems { groups, items })
    }
}

/// The layouts and skills defined inline in a layout tree, each once by name,
/// with the root layout first.
#[derive(Debug, Default)]
pub(crate) struct InlineDefinitions<'a> {
    pub layouts: Vec<&'a AgentLayout>,
    pub skills: Vec<&'a Skill>,
    /// Whether any inclusion is by name, so registered names are needed.
    pub has_named: bool,
}

impl<'a> InlineDefinitions<'a> {
    /// Collect the definitions in `root`. An item defined twice must be equal.
    pub fn collect(root: &'a AgentLayout) -> Result<Self> {
        let mut definitions = Self::default();
        definitions.append_layout(root)?;
        Ok(definitions)
    }

    fn append_layout(&mut self, layout: &'a AgentLayout) -> Result<()> {
        if let Some(existing) = self.layouts.iter().find(|item| item.name == layout.name) {
            if *existing != layout {
                return Err(Error::ConflictingConfig(layout.name.clone()));
            }
            // Already visited, children included.
            return Ok(());
        }
        self.layouts.push(layout);

        for subagent in &layout.subagents.items {
            match subagent {
                AgentConfigRef::Named(_) => self.has_named = true,
                AgentConfigRef::Layout(layout) => self.append_layout(layout)?,
            }
        }
        for skill in &layout.skills.items {
            match skill {
                SkillRef::Named(_) => self.has_named = true,
                SkillRef::Skill(skill) => self.append_skill(skill)?,
            }
        }
        Ok(())
    }

    fn append_skill(&mut self, skill: &'a Skill) -> Result<()> {
        match self.skills.iter().find(|item| item.name() == skill.name()) {
            Some(existing) if *existing != skill => {
                Err(Error::ConflictingSkill(skill.name().to_owned()))
            }
            Some(_) => Ok(()),
            None => {
                self.skills.push(skill);
                Ok(())
            }
        }
    }
}

/// Registered agent config and skill identities, for resolving names.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegisteredNames {
    pub configs: Vec<ConfigId>,
    pub skills: Vec<ConfigId>,
}

/// Find `name` in `group`, then in the global group.
pub(crate) fn resolve_name(
    registered: &[ConfigId],
    group: &ConfigGroup,
    name: &str,
) -> Option<ConfigId> {
    [group, &ConfigGroup::Global].into_iter().find_map(|group| {
        registered
            .iter()
            .find(|id| id.config_group == *group && id.name == name)
            .cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(group: ConfigGroup, name: &str) -> ConfigId {
        ConfigId {
            config_group: group,
            name: name.into(),
        }
    }

    fn own() -> ConfigGroup {
        ConfigGroup::Custom("app".into())
    }

    #[test]
    fn names_resolve_in_the_own_group_before_global() {
        let registered = [
            id(ConfigGroup::Global, "reviewer"),
            id(own(), "reviewer"),
            id(ConfigGroup::Global, "planner"),
            id(ConfigGroup::Custom("other".into()), "writer"),
        ];

        assert_eq!(
            resolve_name(&registered, &own(), "reviewer"),
            Some(id(own(), "reviewer"))
        );
        assert_eq!(
            resolve_name(&registered, &own(), "planner"),
            Some(id(ConfigGroup::Global, "planner"))
        );
        assert_eq!(resolve_name(&registered, &own(), "writer"), None);
    }

    #[test]
    fn layouts_build_config_input_in_the_context_group() {
        let registered = RegisteredNames {
            configs: vec![id(ConfigGroup::Global, "reviewer")],
            skills: vec![id(own(), "rust")],
        };
        let layout = AgentLayout::new("coder")
            .description("Writes code")
            .selectable()
            .bundles(vec![])
            .subagents(Inclusions::default().include_group().include("reviewer"))
            .skills(Inclusions::default().include_global().include("rust"));

        let input = layout.build_config_input(&own(), &registered).unwrap();

        assert_eq!(input.config_id, id(own(), "coder"));
        assert_eq!(input.description, "Writes code");
        assert!(input.selectable);
        assert_eq!(input.models, vec![ModelSelection::Global]);
        assert_eq!(input.bundles, Some(vec![]));
        assert_eq!(
            input.subagents,
            IncludedItems {
                groups: vec![own()],
                items: vec![id(ConfigGroup::Global, "reviewer")],
            }
        );
        assert_eq!(
            input.skills,
            IncludedItems {
                groups: vec![ConfigGroup::Global],
                items: vec![id(own(), "rust")],
            }
        );
    }

    #[test]
    fn inline_definitions_are_collected_once_and_conflicts_fail() {
        let reviewer = AgentLayout::new("reviewer").description("Reviews");
        let layout = AgentLayout::new("coder").subagents(
            Inclusions::default()
                .include(reviewer.clone())
                .include(
                    AgentLayout::new("tester")
                        .subagents(Inclusions::default().include(reviewer.clone())),
                )
                .include("planner"),
        );

        let definitions = InlineDefinitions::collect(&layout).unwrap();
        let names: Vec<_> = definitions
            .layouts
            .iter()
            .map(|layout| layout.name.as_str())
            .collect();
        assert_eq!(names, ["coder", "reviewer", "tester"]);
        assert!(definitions.has_named);

        let conflicting = AgentLayout::new("coder").subagents(
            Inclusions::default()
                .include(reviewer)
                .include(AgentLayout::new("reviewer").description("Different")),
        );
        assert!(matches!(
            InlineDefinitions::collect(&conflicting),
            Err(Error::ConflictingConfig(name)) if name == "reviewer"
        ));
    }

    #[test]
    fn inline_items_resolve_in_the_own_group() {
        let layout = AgentLayout::new("coder")
            .subagents(Inclusions::default().include(AgentLayout::new("reviewer")));
        let registered = RegisteredNames {
            configs: vec![id(ConfigGroup::Global, "reviewer"), id(own(), "reviewer")],
            skills: vec![],
        };

        let input = layout.build_config_input(&own(), &registered).unwrap();

        assert_eq!(input.subagents.items, vec![id(own(), "reviewer")]);
    }

    #[test]
    fn unknown_names_are_reported_by_kind() {
        let layout = AgentLayout::new("coder").skills(Inclusions::default().include("missing"));

        let error = layout
            .build_config_input(&own(), &RegisteredNames::default())
            .unwrap_err();

        assert!(matches!(error, Error::UnknownSkill(name) if name == "missing"));
    }
}
