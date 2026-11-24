//! The agent information window: identity, environment, and token usage, plus
//! the pinned definition's bundles and the tools, hooks, skills, and subagents
//! they give this agent.
use crate::{app::Ui, modal::Modal, network, reactive::use_ui_subscription, views::ProbeName};
use client_shared::*;
use dioxus::prelude::*;
use llm_api::{api::Usage, providers::Effort};
use psa::api::*;
use std::rc::Rc;

#[cfg(test)]
#[path = "tests/agent_info/agent_info_tests.rs"]
mod agent_info_tests;

/// Minute-precision timestamp in the server's offset, normally UTC.
macro_rules! format_timestamp {
    ($at:expr) => {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            $at.year(),
            u8::from($at.month()),
            $at.day(),
            $at.hour(),
            $at.minute()
        )
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfoTab {
    Overview,
    Bundles,
    Tools,
    Hooks,
    Skills,
    Subagents,
}

impl InfoTab {
    const ALL: [Self; 6] = [
        Self::Overview,
        Self::Bundles,
        Self::Tools,
        Self::Hooks,
        Self::Skills,
        Self::Subagents,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Bundles => "Bundles",
            Self::Tools => "Tools",
            Self::Hooks => "Hooks",
            Self::Skills => "Skills",
            Self::Subagents => "Subagents",
        }
    }
}

/// Server state behind the definition tabs, fetched when the window opens.
/// Each part fails independently so one unavailable route hides only its tab.
struct DefinitionDetails {
    config: Result<ResolvedAgentConfig, String>,
    components: Result<AgentComponents, String>,
    skills: Result<Vec<Skill>, String>,
    model_usage: Result<Vec<ModelUsage>, String>,
    subagents: Vec<(AgentDefinitionId, Result<ResolvedAgentConfig, String>)>,
    /// Model and context size when no open transcript supplies them.
    context: Option<AgentContextState>,
}

impl DefinitionDetails {
    fn unavailable(message: String) -> Self {
        Self {
            config: Err(message.clone()),
            components: Err(message.clone()),
            skills: Err(message.clone()),
            model_usage: Err(message),
            subagents: Vec::new(),
            context: None,
        }
    }

    fn tab_count(&self, tab: InfoTab) -> Option<usize> {
        let components = self.components.as_ref().ok();
        let sum = |count: fn(&InstalledBundle) -> usize| {
            components.map(|c| c.bundles.iter().map(count).sum::<usize>())
        };
        match tab {
            InfoTab::Overview => None,
            InfoTab::Bundles => self
                .config
                .as_ref()
                .ok()
                .map(|config| config.definition.bundles.len()),
            InfoTab::Tools => sum(|b| b.tools.len() + b.codemode_functions.len()),
            InfoTab::Hooks => sum(|b| b.hooks.len()),
            InfoTab::Skills => self
                .config
                .as_ref()
                .ok()
                .map(|config| config.references.skills.len()),
            InfoTab::Subagents => Some(self.subagents.len()),
        }
    }
}

fn error_text(error: ClientError) -> String {
    error.to_string()
}

async fn load_details(ui: Signal<Ui>, target: TranscriptTarget) -> DefinitionDetails {
    let api = match network::client(ui) {
        Ok(api) => api,
        Err(error) => return DefinitionDetails::unavailable(error.to_string()),
    };
    let known = ui
        .peek()
        .agents
        .get(&target.session_id)
        .and_then(|agents| agents.iter().find(|agent| agent.id == target.agent_id))
        .map(|agent| agent.definition_id);
    let definition_id = match known {
        Some(id) => id,
        None => match api.get_agent(target.session_id, target.agent_id).await {
            Ok(agent) => agent.definition_id,
            Err(error) => return DefinitionDetails::unavailable(error_text(error)),
        },
    };

    let (config, components, skills, model_usage, context) = futures_util::join!(
        api.get_resolved_agent_config(definition_id),
        api.get_agent_components(target.session_id, target.agent_id),
        api.list_skills(),
        api.get_agent_model_usage(target.session_id, target.agent_id),
        api.get_agent_context_state(target.session_id, target.agent_id),
    );
    let config = config.map_err(error_text);

    // Keep the definition's order; records missing from the list are dropped.
    let skills = match (&config, skills) {
        (Ok(config), Ok(all)) => Ok(config
            .references
            .skills
            .iter()
            .filter_map(|id| all.iter().find(|skill| skill.id == *id).cloned())
            .collect()),
        (_, Err(error)) => Err(error_text(error)),
        (Err(error), _) => Err(error.clone()),
    };

    let subagent_ids = config
        .as_ref()
        .map(|config| config.references.subagents.clone())
        .unwrap_or_default();
    let subagents = futures_util::future::join_all(subagent_ids.into_iter().map(|id| {
        let api = &api;
        async move {
            (
                id,
                api.get_resolved_agent_config(id).await.map_err(error_text),
            )
        }
    }))
    .await;

    DefinitionDetails {
        config,
        components: components.map_err(error_text),
        skills,
        model_usage: model_usage.map_err(error_text),
        subagents,
        context: context.ok(),
    }
}

/// Information about an existing agent, opened by `/info` or the agent list.
#[component]
pub(crate) fn AgentInfo(target: TranscriptTarget, onclose: EventHandler<()>) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut tab = use_signal(|| InfoTab::Overview);
    let mut details = use_signal(|| None::<Rc<DefinitionDetails>>);
    use_hook(move || {
        spawn(async move {
            let loaded = load_details(ui, target).await;
            details.set(Some(Rc::new(loaded)));
        });
    });

    use_ui_subscription(move |state| {
        (
            state.agents.get(&target.session_id).cloned(),
            state.sessions.clone(),
            state.transcripts.get(&target).cloned(),
            state.models.clone(),
        )
    });
    let state = ui.peek();
    let agents = state.agents.get(&target.session_id);
    let agent = agents
        .and_then(|agents| agents.iter().find(|agent| agent.id == target.agent_id))
        .cloned();
    let parent = agent
        .as_ref()
        .and_then(|agent| agent.subagent_parent_id)
        .and_then(|id| agents?.iter().find(|agent| agent.id == id))
        .map(|parent| parent.name.clone());
    let session = state
        .sessions
        .iter()
        .find(|session| session.id == target.session_id)
        .map(|session| {
            (
                session.working_directory.display().to_string(),
                session.probe_id,
            )
        });
    let status = state
        .transcripts
        .get(&target)
        .map(|transcript| agent_status(transcript, &state.models));
    let models = state.models.clone();
    drop(state);

    let loaded = details.read().clone();
    let name = agent
        .as_ref()
        .map(|agent| agent.name.clone())
        .unwrap_or_else(|| "Unknown agent".into());

    // An open transcript is live; otherwise use the snapshot fetched on open.
    let (model_config, context_tokens) = match &status {
        Some(status) => (status.model_config.clone(), status.context_tokens),
        None => loaded
            .as_ref()
            .and_then(|details| details.context.clone())
            .map(|context| (context.model_config, context.context_tokens))
            .unwrap_or_default(),
    };
    let context_window = model_config.as_ref().and_then(|config| {
        models
            .iter()
            .find(|info| info.model == config.model)
            .and_then(|info| info.context_window)
    });
    let model = model_config
        .as_ref()
        .map(|config| crate::composer::model_display_name(&models, &config.model));
    let effort = model_config.as_ref().map(|config| config.effort);

    let active = tab();
    let body = match (active, loaded.as_deref()) {
        (InfoTab::Overview, details) => rsx! {
            {render_overview(Overview {
                agent: agent.as_ref(),
                parent,
                details,
                path: session.as_ref().map(|s| s.0.clone()),
                probe: session.as_ref().map(|s| s.1),
                model, effort, context_tokens, context_window,
            })}
            if let Some(details) = details { {render_model_usage(details, &models)} }
        },
        (_, None) => rsx! { p { class: "muted info-loading", "Loading definition…" } },
        (InfoTab::Bundles, Some(details)) => render_bundles(details),
        (InfoTab::Tools, Some(details)) => render_tools(details),
        (InfoTab::Hooks, Some(details)) => render_hooks(details),
        (InfoTab::Skills, Some(details)) => render_skills(details),
        (InfoTab::Subagents, Some(details)) => render_subagents(details),
    };

    rsx! {
        Modal { title: "Agent information", close_label: "Close agent information", class: "agent-info agent-info-wide", onclose: move |_| onclose.call(()),
            div { class: "info-title",
                h3 { "{name}" }
                if let Some(agent) = &agent { {render_state(agent.state)} }
            }
            nav { class: "info-tabs", role: "tablist", "aria-label": "Agent information sections",
                for item in InfoTab::ALL {
                    button { r#type: "button", role: "tab",
                        class: if item == active { "active" } else { "" },
                        "aria-selected": item == active,
                        onclick: move |_| tab.set(item),
                        "{item.label()}"
                        if let Some(count) = loaded.as_ref().and_then(|details| details.tab_count(item)) {
                            span { class: "info-tab-count", "{count}" }
                        }
                    }
                }
            }
            // A keyed wrapper replaces the scroll position when the tab changes.
            for key in [active.label()] {
                div { key: "{key}", class: "info-panel", role: "tabpanel", {body.clone()} }
            }
        }
    }
}

/// Information about the root agent a new session will create.
#[component]
pub(crate) fn DraftAgentInfo(
    name: String,
    template: Option<String>,
    path: Option<String>,
    probe: Option<ProbeId>,
    model: Option<String>,
    effort: Option<Effort>,
    onclose: EventHandler<()>,
) -> Element {
    rsx! {
        Modal { title: "Agent information", close_label: "Close agent information", class: "agent-info", onclose: move |_| onclose.call(()),
            div { class: "info-title", h3 { "{name}" } span { class: "info-state", "Not created" } }
            section { class: "info-section",
                h4 { "Environment" }
                dl { class: "info-grid",
                    dt { "Configuration" } dd { "{template.as_deref().unwrap_or(\"Unknown\")}" }
                    {render_environment(path, probe, model, effort, None, None)}
                }
            }
            section { class: "info-section",
                h4 { "Tokens" }
                UsageGrid { usage: None }
            }
        }
    }
}

/// What the overview tab shows besides the agent's own record.
struct Overview<'a> {
    agent: Option<&'a Agent>,
    parent: Option<String>,
    details: Option<&'a DefinitionDetails>,
    path: Option<String>,
    probe: Option<ProbeId>,
    model: Option<String>,
    effort: Option<Effort>,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
}

fn render_overview(overview: Overview<'_>) -> Element {
    let Overview {
        agent,
        parent,
        details,
        path,
        probe,
        model,
        effort,
        context_tokens,
        context_window,
    } = overview;
    let config = details.and_then(|details| details.config.as_ref().ok());
    let config_failed = details.is_some_and(|details| details.config.is_err());
    let description = config
        .as_ref()
        .map(|config| config.definition.description.clone())
        .filter(|description| !description.trim().is_empty());

    rsx! {
        if let Some(description) = description { p { class: "info-description", "{description}" } }
        if let Some(agent) = agent {
            div { class: "info-badges",
                span { class: "info-badge", if agent.interactive { "Interactive" } else { "Noninteractive" } }
                if agent.hidden { span { class: "info-badge", "Internal" } }
                if let Some(parent) = &parent { span { class: "info-badge", "Subagent of {parent}" } }
                else if agent.subagent_parent_id.is_some() { span { class: "info-badge", "Subagent" } }
                if agent.forked_from.is_some() { span { class: "info-badge", "Forked" } }
            }
        }
        div { class: "info-columns",
            section { class: "info-section",
                h4 { "Definition" }
                dl { class: "info-grid",
                    dt { "Configuration" }
                    dd {
                        if let Some(config) = config { "{config.config.config_id.name}" }
                        else if config_failed { "Unknown" }
                        else { span { class: "muted", "Loading…" } }
                    }
                    if let Some(config) = config {
                        dt { "Source" } dd { "{config_group_label(&config.config.config_id.config_group)}" }
                        dt { "Model policy" } dd { "{model_policy_label(&config.definition.model)}" }
                    }
                    if let Some(agent) = agent {
                        dt { "Snapshot" } dd { "#{agent.definition_id.0}" }
                    }
                }
            }
            section { class: "info-section",
                h4 { "Environment" }
                dl { class: "info-grid", {render_environment(path, probe, model, effort, context_tokens, context_window)} }
            }
        }
        section { class: "info-section",
            h4 { "Tokens" }
            UsageGrid { usage: agent.map(|agent| agent.usage.clone()), cacheable_input: agent.and_then(|agent| agent.cacheable_input_tokens) }
        }
        if let Some(agent) = agent {
            p { class: "info-footer muted",
                "Agent #{agent.id.0} · created {format_timestamp!(agent.created_at)} · updated {format_timestamp!(agent.updated_at)}"
            }
        }
    }
}

fn render_environment(
    path: Option<String>,
    probe: Option<ProbeId>,
    model: Option<String>,
    effort: Option<Effort>,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
) -> Element {
    let context = match (context_tokens, context_window) {
        (Some(tokens), Some(window)) if window > 0 => Some(format!(
            "{} / {} ({}%)",
            group_digits(tokens),
            group_digits(window),
            (u128::from(tokens) * 100 + u128::from(window) / 2) / u128::from(window)
        )),
        (Some(tokens), _) => Some(group_digits(tokens)),
        _ => None,
    };
    rsx! {
        dt { "Path" } dd { class: "info-path", "{path.as_deref().unwrap_or(\"Unknown\")}" }
        dt { "Probe" } dd { if let Some(id) = probe { ProbeName { id } } else { "Unknown" } }
        dt { "Model" }
        dd {
            "{model.as_deref().unwrap_or(\"Unknown\")}"
            " · "
            span { class: effort.map(|e| format!("effort-{}", e.as_str())).unwrap_or_default(), "{effort.map(|e| e.as_str()).unwrap_or(\"Unknown\")}" }
        }
        if let Some(context) = context {
            dt { title: "Last inference total; not the exact current context", "Context" } dd { "{context}" }
        }
    }
}

fn render_state(state: AgentState) -> Element {
    let (class, label) = match state {
        AgentState::Idle => ("idle", "Idle"),
        AgentState::Running => ("running", "Running"),
        AgentState::Interrupted => ("interrupted", "Interrupted"),
    };
    rsx! { span { class: "info-state info-state-{class}", "{label}" } }
}

fn config_group_label(group: &ConfigGroup) -> String {
    match group {
        ConfigGroup::Global => "Global configuration".into(),
        ConfigGroup::Custom(name) => format!("Custom group “{name}”"),
        ConfigGroup::AgentsDir { path, .. } => format!("Project agents at {path}"),
        ConfigGroup::Session => "Session agents directory".into(),
    }
}

fn model_policy_label(model: &AgentModel) -> String {
    match model {
        AgentModel::Global => "Follows the session".into(),
        AgentModel::Fixed { model, effort } => {
            let effort = match effort {
                AgentEffort::Global => "session effort".to_owned(),
                AgentEffort::Fixed(effort) => effort.as_str().to_owned(),
            };
            format!("Pinned to {} · {effort}", model.model_name())
        }
    }
}

pub(crate) fn token_difference(total: Option<u64>, part: Option<u64>) -> Option<u64> {
    total?.checked_sub(part?)
}

/// Formats a count with comma thousands separators, such as `1,234,567`.
pub(crate) fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// Cumulative token counts with their cache and reasoning breakdowns.
#[component]
pub(crate) fn UsageGrid(
    usage: Option<Usage>,
    /// Input a cache could have served; the hit rate's denominator.
    #[props(default)]
    cacheable_input: Option<u64>,
) -> Element {
    let count = |value: Option<u64>| value.map(group_digits).unwrap_or_else(|| "Unknown".into());
    let input = usage.as_ref().and_then(|u| u.input_tokens);
    let cached = usage.as_ref().and_then(|u| u.cached_input_tokens);
    let cache_write = usage.as_ref().and_then(|u| u.cache_write_tokens);
    let hit = usage
        .as_ref()
        .and_then(|usage| cache_hit_rate(usage, cacheable_input))
        .map(|rate| format!("{:.1}%", rate * 100.0))
        .unwrap_or_else(|| "Unknown".into());
    let output = usage.as_ref().and_then(|u| u.output_tokens);
    let thinking = usage.as_ref().and_then(|u| u.reasoning_output_tokens);
    rsx! {
        div { class: "usage-columns",
            dl { class: "info-grid",
                dt { "Input tokens" } dd { class: "info-number", "{count(input)}" }
                dt { class: "info-detail", "Cached" } dd { class: "info-number", "{count(cached)}" }
                dt { class: "info-detail", "Cacheable" } dd { class: "info-number", "{count(cacheable_input)}" }
                dt { class: "info-detail", "Cache hit rate" } dd { class: "info-number", "{hit}" }
                dt { class: "info-detail", "Uncached" } dd { class: "info-number", "{count(token_difference(input, cached))}" }
                if let Some(cache_write) = cache_write { dt { class: "info-detail", "Cache write" } dd { class: "info-number", "{group_digits(cache_write)}" } }
            }
            dl { class: "info-grid",
                dt { "Output tokens" } dd { class: "info-number", "{count(output)}" }
                dt { class: "info-detail", "Thinking" } dd { class: "info-number", "{count(thinking)}" }
                dt { class: "info-detail", "Non-thinking" } dd { class: "info-number", "{count(token_difference(output, thinking))}" }
                dt { class: "info-total", "Total tokens" } dd { class: "info-number info-total", "{count(usage.as_ref().and_then(|u| u.total_tokens))}" }
            }
        }
    }
}

fn render_model_usage(details: &DefinitionDetails, models: &[ModelInfo]) -> Element {
    let Ok(usage) = &details.model_usage else {
        return rsx! {};
    };
    if usage.len() < 2 {
        return rsx! {};
    }
    let count = |value: Option<u64>| value.map(group_digits).unwrap_or_else(|| "?".into());
    rsx! {
        section { class: "info-section",
            h4 { "Usage by model" }
            table { class: "info-table",
                thead { tr { th { "Model" } th { "Input" } th { "Output" } th { "Total" } } }
                tbody {
                    for entry in usage {
                        tr {
                            td { title: "{entry.model}", "{crate::composer::model_display_name(models, &entry.model)}" }
                            td { "{count(entry.usage.input_tokens)}" }
                            td { "{count(entry.usage.output_tokens)}" }
                            td { "{count(entry.usage.total_tokens)}" }
                        }
                    }
                }
            }
        }
    }
}

fn render_failure(message: &str) -> Element {
    rsx! { p { class: "error", "Unavailable: {message}" } }
}

/// What a bundle provides, independent of how it is configured.
fn bundle_summary(id: BundleId) -> &'static str {
    match id {
        BundleId::Startup => {
            "Starts a new context with the model, instructions, and tool definitions"
        }
        BundleId::StopWithoutTools => "Finishes a run after a response without tool calls",
        BundleId::ExtraSystemPrompt => "Adds a configured system prompt when the agent is created",
        BundleId::FileTools => "File reading and editing tools",
        BundleId::Bash => "Shell commands with persistent process sessions",
        BundleId::CodeMode => "A Python REPL that can call other tools and functions",
        BundleId::Goal => "Goal tools and a goal-aware stopping policy",
        BundleId::AskQuestion => "Asks a connected client for structured input",
        BundleId::Subagents => "Creates and messages other agents in the session",
        BundleId::TypedResult => "Records a schema-validated result before finishing",
        BundleId::Compaction => "Summarizes older context near the context window",
        BundleId::Traces => "Lists sessions and exports them as traces",
        BundleId::DecisionModels => "CodeMode functions for calling decision models",
    }
}

fn field_label(key: &str) -> String {
    let mut label = key.replace('_', " ");
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    label
}

/// A bundle's construction parameters, as name and value pairs.
fn bundle_parameters(config: &BundleConfig) -> Vec<(String, serde_json::Value)> {
    // Configurations are externally tagged; unit variants carry no fields.
    let Ok(serde_json::Value::Object(tagged)) = serde_json::to_value(config) else {
        return Vec::new();
    };
    let Some(serde_json::Value::Object(fields)) = tagged.into_iter().next().map(|(_, v)| v) else {
        return Vec::new();
    };
    fields
        .into_iter()
        .map(|(key, value)| (field_label(&key), value))
        .collect()
}

fn render_parameter(value: &serde_json::Value) -> Element {
    match value {
        serde_json::Value::Bool(true) => rsx! { span { class: "param-on", "Yes" } },
        serde_json::Value::Bool(false) => rsx! { span { class: "param-off", "No" } },
        serde_json::Value::Null => rsx! { span { class: "muted", "Default" } },
        serde_json::Value::Number(number) => rsx! { "{number}" },
        serde_json::Value::String(text) if text.len() <= 60 && !text.contains('\n') => {
            rsx! { "{text}" }
        }
        serde_json::Value::Array(items) if items.is_empty() => {
            rsx! { span { class: "muted", "None" } }
        }
        other => {
            let text = match other {
                serde_json::Value::String(text) => text.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            let lines = text.lines().count();
            rsx! {
                details { class: "param-long",
                    summary { if lines > 1 { "{lines} lines" } else { "{text.chars().count()} characters" } }
                    pre { "{text}" }
                }
            }
        }
    }
}

fn render_bundles(details: &DefinitionDetails) -> Element {
    let configs = match &details.config {
        Ok(config) => config.definition.bundles.clone(),
        Err(error) => return render_failure(error),
    };
    if configs.is_empty() {
        return rsx! { p { class: "muted", "This definition installs no bundles." } };
    }
    let installed = details.components.as_ref().ok();
    rsx! {
        if let Err(error) = &details.components {
            p { class: "muted", "Installed components are unavailable: {error}" }
        }
        ol { class: "bundle-list",
            for (index, config) in configs.into_iter().enumerate() {
                {
                    let parameters = bundle_parameters(&config);
                    let components = installed.and_then(|c| c.bundles.get(index)).filter(|b| b.config.id() == config.id());
                    let empty = components.is_some_and(|b| b.tools.is_empty() && b.hooks.is_empty() && b.codemode_functions.is_empty());
                    rsx! {
                        li { class: "bundle-card",
                            header {
                                span { class: "bundle-order", "{index + 1}" }
                                h4 { "{config.id():?}" }
                            }
                            p { class: "muted bundle-summary", "{bundle_summary(config.id())}" }
                            if !parameters.is_empty() {
                                dl { class: "bundle-params",
                                    for (label, value) in parameters {
                                        dt { "{label}" } dd { {render_parameter(&value)} }
                                    }
                                }
                            }
                            if let Some(components) = components {
                                if empty {
                                    p { class: "muted bundle-empty", "Installs nothing for this agent." }
                                } else {
                                    div { class: "component-chips",
                                        for tool in &components.tools { span { class: "chip chip-tool", title: "Tool", "{tool.name}" } }
                                        for function in &components.codemode_functions { span { class: "chip chip-function", title: "CodeMode function", "{function.name}()" } }
                                        for hook in &components.hooks { span { class: "chip chip-hook", title: "{stage_label(hook.stage)} hook", "{hook.name}" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn bundle_name(bundle: &InstalledBundle) -> String {
    format!("{:?}", bundle.config.id())
}

fn render_tools(details: &DefinitionDetails) -> Element {
    let components = match &details.components {
        Ok(components) => components,
        Err(error) => return render_failure(error),
    };
    let tools: Vec<_> = components
        .bundles
        .iter()
        .flat_map(|bundle| {
            bundle
                .tools
                .iter()
                .map(move |tool| (bundle_name(bundle), tool))
        })
        .collect();
    let functions: Vec<_> = components
        .bundles
        .iter()
        .flat_map(|bundle| {
            bundle
                .codemode_functions
                .iter()
                .map(move |function| (bundle_name(bundle), function))
        })
        .collect();
    rsx! {
        section { class: "info-section",
            h4 { "Model tools" }
            if tools.is_empty() { p { class: "muted", "No model-facing tools." } }
            div { class: "info-items",
                for (bundle, tool) in tools {
                    details { class: "info-item",
                        summary {
                            code { "{tool.name}" }
                            span { class: "info-item-preview", "{first_line(&tool.description)}" }
                            span { class: "chip", "{bundle}" }
                        }
                        p { class: "info-item-body", "{tool.description}" }
                    }
                }
            }
        }
        if !functions.is_empty() {
            section { class: "info-section",
                h4 { "CodeMode functions" }
                div { class: "info-items",
                    for (bundle, function) in functions {
                        details { class: "info-item",
                            summary {
                                code { "{function.name}()" }
                                span { class: "info-item-preview", "{first_line(&function.description)}" }
                                if function.help_only { span { class: "chip", title: "Listed only by help(), not in the system prompt", "help only" } }
                                span { class: "chip", "{bundle}" }
                            }
                            p { class: "info-item-body", "{function.description}" }
                        }
                    }
                }
            }
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn stage_label(stage: HookStage) -> &'static str {
    match stage {
        HookStage::AgentCreate => "Agent creation",
        HookStage::SessionUpdate => "Session update",
        HookStage::BeforeRequest => "Before inference",
        HookStage::AfterRequest => "After inference",
        HookStage::Command => "Command",
    }
}

fn stage_description(stage: HookStage) -> &'static str {
    match stage {
        HookStage::AgentCreate => "Runs once when the agent is created; forks inherit its effects.",
        HookStage::SessionUpdate => "Runs when session settings, such as the model, change.",
        HookStage::BeforeRequest => "Runs before every inference request.",
        HookStage::AfterRequest => {
            "Runs after every inference request and decides whether to continue."
        }
        HookStage::Command => {
            "Handles submitted commands; the first hook that accepts one runs it."
        }
    }
}

fn render_hooks(details: &DefinitionDetails) -> Element {
    let components = match &details.components {
        Ok(components) => components,
        Err(error) => return render_failure(error),
    };
    let stages = [
        HookStage::AgentCreate,
        HookStage::SessionUpdate,
        HookStage::BeforeRequest,
        HookStage::AfterRequest,
        HookStage::Command,
    ];
    rsx! {
        p { class: "muted info-note", "Hooks in each stage run in the order listed." }
        div { class: "hook-stages",
            for stage in stages {
                {
                    let hooks: Vec<_> = components.bundles.iter()
                        .flat_map(|bundle| bundle.hooks.iter().filter(move |hook| hook.stage == stage).map(move |hook| (bundle_name(bundle), hook.name.clone())))
                        .collect();
                    rsx! {
                        section { class: "hook-stage",
                            header { h4 { "{stage_label(stage)}" } span { class: "info-tab-count", "{hooks.len()}" } }
                            p { class: "muted", "{stage_description(stage)}" }
                            if hooks.is_empty() {
                                p { class: "muted hook-none", "None" }
                            } else {
                                ol { class: "hook-list",
                                    for (bundle, name) in hooks {
                                        li { span { class: "hook-name", "{name}" } span { class: "chip", "{bundle}" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn skill_location(location: &SkillLocation) -> String {
    match location {
        SkillLocation::Global(path) => format!("Server · {}", path.display()),
        SkillLocation::Probe { path, .. } => format!("Probe · {}", path.display()),
    }
}

fn render_skills(details: &DefinitionDetails) -> Element {
    let config = match &details.config {
        Ok(config) => config,
        Err(error) => return render_failure(error),
    };
    let catalogue_off = config
        .definition
        .bundles
        .iter()
        .any(|bundle| matches!(bundle, BundleConfig::Startup { skills: false, .. }));
    let skills = match &details.skills {
        Ok(skills) => skills,
        Err(error) => return render_failure(error),
    };
    rsx! {
        if catalogue_off {
            p { class: "muted info-note", "Startup does not add the skill catalogue to this agent's context." }
        }
        if skills.is_empty() {
            p { class: "muted", "This definition includes no skills." }
        }
        div { class: "info-items",
            for skill in skills {
                details { class: "info-item",
                    summary {
                        code { "{skill.name}" }
                        span { class: "info-item-preview", "{first_line(&skill.description)}" }
                        if skill.disable_model_invocation { span { class: "chip", title: "Excluded from the model-facing catalogue", "manual only" } }
                    }
                    div { class: "info-item-body",
                        p { "{skill.description}" }
                        p { class: "muted info-path", "{skill_location(&skill.skill_location)}" }
                    }
                }
            }
        }
    }
}

fn render_subagents(details: &DefinitionDetails) -> Element {
    let has_tools = details.config.as_ref().is_ok_and(|config| {
        config
            .definition
            .bundles
            .iter()
            .any(|bundle| bundle.id() == BundleId::Subagents)
    });
    rsx! {
        if details.config.is_ok() && !has_tools {
            p { class: "muted info-note", "Without the Subagents bundle, this agent cannot create the agents below." }
        }
        if details.subagents.is_empty() {
            p { class: "muted", "No subagent definitions are permitted." }
        }
        div { class: "info-items",
            for (id, config) in details.subagents.iter() {
                match config {
                    Ok(config) => rsx! {
                        div { class: "info-item info-item-static",
                            div { class: "info-item-heading",
                                code { "{config.config.config_id.name}" }
                                span { class: "chip", title: "Definition snapshot", "#{id.0}" }
                            }
                            if !config.definition.description.trim().is_empty() {
                                p { class: "info-item-body", "{config.definition.description}" }
                            }
                        }
                    },
                    Err(error) => rsx! {
                        div { class: "info-item info-item-static",
                            code { "#{id.0}" }
                            p { class: "error", "{error}" }
                        }
                    },
                }
            }
        }
    }
}
