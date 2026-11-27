//! The root agent a new session will create: its configuration and model
//! policy, chosen before the first message.
use psa::api::{AgentConfigStatus, ConfigId, EffortSelection, ModelSelection};

#[derive(Debug, Clone, Default)]
pub struct DraftRoot {
    pub configs: Vec<AgentConfigStatus>,
    pub selected: Option<ConfigId>,
    pub model: Option<ModelSelection>,
    pub loading: bool,
    pub ready: bool,
    pub pending: bool,
    pub uncertain: bool,
}

impl DraftRoot {
    /// Keep the usable configurations of `items`, choosing the server's
    /// default unless an earlier choice is still offered.
    pub fn apply_configs(&mut self, items: Vec<AgentConfigStatus>, default_name: &str) {
        let previous_config = self.selected.clone();
        let previous_model = self.model.clone();
        self.configs = items
            .into_iter()
            .filter(|c| {
                c.config.selectable
                    && c.status.missing_components.is_empty()
                    && !c.config.models.is_empty()
            })
            .collect();
        let mut defaults = self
            .configs
            .iter()
            .filter(|c| c.config.config_id.name == default_name);
        let first = defaults.next().map(|c| c.config.config_id.clone());
        let default = if defaults.next().is_none() {
            first
        } else {
            None
        };
        let preserved =
            previous_config.filter(|id| self.configs.iter().any(|c| c.config.config_id == *id));
        self.selected = None;
        self.model = None;
        if let Some(id) = preserved.clone().or(default) {
            self.select_config(id);
            if preserved.is_some() {
                // Preserve an explicitly selected wire policy only if it is still allowed.
                if previous_model.as_ref().is_some_and(|model| {
                    self.configs.iter().any(|c| {
                        Some(&c.config.config_id) == self.selected.as_ref()
                            && c.config.models.contains(model)
                    })
                }) {
                    self.model = previous_model;
                }
            }
        }
    }

    /// Return one permitted wire selector for each distinct root model policy.
    pub fn model_choices(&self) -> Vec<ModelSelection> {
        let Some(config) = self
            .configs
            .iter()
            .find(|c| Some(&c.config.config_id) == self.selected.as_ref())
        else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for selection in &config.config.models {
            if !choices
                .iter()
                .any(|other| root_models_match(selection, other))
            {
                choices.push(selection.clone());
            }
        }
        choices
    }

    pub fn select_config(&mut self, id: ConfigId) {
        self.selected = Some(id);
        let choices = self.model_choices();
        let has_default = self.configs.iter().any(|c| {
            Some(&c.config.config_id) == self.selected.as_ref() && c.config.first_model_default
        });
        self.model = if has_default || choices.len() == 1 {
            choices.into_iter().next()
        } else {
            None
        };
    }
}

fn root_models_match(left: &ModelSelection, right: &ModelSelection) -> bool {
    match (left, right) {
        (
            ModelSelection::Global | ModelSelection::Parent,
            ModelSelection::Global | ModelSelection::Parent,
        ) => true,
        (
            ModelSelection::Fixed {
                model: left_model,
                effort: left_effort,
            },
            ModelSelection::Fixed {
                model: right_model,
                effort: right_effort,
            },
        ) => {
            left_model == right_model
                && (left_effort == right_effort
                    || matches!(
                        (left_effort, right_effort),
                        (
                            EffortSelection::Global | EffortSelection::Parent,
                            EffortSelection::Global | EffortSelection::Parent
                        )
                    ))
        }
        _ => false,
    }
}

/// A root model policy as a menu shows it.
pub fn draft_model_label(model: &ModelSelection) -> String {
    match model {
        ModelSelection::Global | ModelSelection::Parent => {
            "Session default model and effort".into()
        }
        ModelSelection::Fixed { model, effort } => {
            let effort = match effort {
                EffortSelection::Global | EffortSelection::Parent => "session effort".to_string(),
                EffortSelection::Fixed(effort) => effort.as_str().to_string(),
            };
            format!("{model} · {effort}")
        }
    }
}
