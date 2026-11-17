//! Permitted child templates and their model choices, as presented to the model.
//!
//! Loaded once at installation from the caller's pinned definition so that the
//! create_agent schema, its validation, and the identity prompt agree.

use crate::agents::sessions::Sessions;
use psa::api::{AgentDefinitionId, EffortSelection, ModelSelection};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// Short model-facing value accepted by create_agent's `model` argument.
    pub label: String,
    pub selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTemplate {
    pub definition_id: AgentDefinitionId,
    pub name: String,
    pub description: String,
    /// Empty when the definition has no configuration snapshot; the definition's
    /// own model policy then applies and no choice is accepted.
    pub models: Vec<ModelChoice>,
    /// Whether the first model applies when no choice is supplied.
    pub default: bool,
}

impl ChildTemplate {
    pub fn has_model_choice(&self) -> bool {
        self.models.len() > 1
    }

    pub fn requires_model_choice(&self) -> bool {
        self.has_model_choice() && !self.default
    }

    /// Map a model-facing label to its selection; `None` keeps the default.
    pub fn resolve_model(&self, label: Option<&str>) -> Result<Option<ModelSelection>, String> {
        let Some(label) = label else {
            if self.requires_model_choice() {
                return Err(format!(
                    "Template \"{}\" requires a model. Choose one of: {}",
                    self.name,
                    self.model_labels()
                ));
            }
            return Ok(None);
        };
        if self.models.is_empty() {
            return Err(format!(
                "Template \"{}\" has a fixed model; omit model",
                self.name
            ));
        }
        self.models
            .iter()
            .find(|choice| choice.label == label)
            .map(|choice| Some(choice.selection.clone()))
            .ok_or_else(|| {
                format!(
                    "Model \"{label}\" is not allowed for template \"{}\". Choose one of: {}",
                    self.name,
                    self.model_labels()
                )
            })
    }

    fn model_labels(&self) -> String {
        self.models
            .iter()
            .map(|choice| choice.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// One prompt line describing this template and its model policy.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let description = self.description.trim();
        if !description.is_empty() {
            let period = if description.ends_with(['.', '!', '?']) {
                ""
            } else {
                "."
            };
            parts.push(format!("{description}{period}"));
        }
        match self.models.as_slice() {
            [] => {}
            [only] => parts.push(format!("Model: {}.", describe_model(&only.selection))),
            choices => {
                let listed: Vec<_> = choices
                    .iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        let default = if index == 0 && self.default {
                            ", default"
                        } else {
                            ""
                        };
                        format!(
                            "\"{}\" ({}{default})",
                            choice.label,
                            describe_model(&choice.selection)
                        )
                    })
                    .collect();
                parts.push(format!("Models: {}.", listed.join(", ")));
            }
        }
        match parts.is_empty() {
            true => format!("- {}", self.name),
            false => format!("- {}: {}", self.name, parts.join(" ")),
        }
    }
}

/// Find a template by name, defaulting when exactly one is permitted.
pub fn select_template<'a>(
    templates: &'a [ChildTemplate],
    name: Option<&str>,
) -> Result<&'a ChildTemplate, String> {
    let available = || {
        templates
            .iter()
            .map(|template| template.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let name = match (name, templates) {
        (_, []) => return Err("This agent may not create child agents".into()),
        (None, [only]) => return Ok(only),
        (None, _) => {
            return Err(format!(
                "agent_template is required. Choose one of: {}",
                available()
            ));
        }
        (Some(name), _) => name,
    };
    let mut matches = templates.iter().filter(|template| template.name == name);
    let selected = matches.next().ok_or_else(|| {
        format!(
            "Unknown agent_template \"{name}\". Choose one of: {}",
            available()
        )
    })?;
    if matches.next().is_some() {
        return Err(format!("Ambiguous agent_template \"{name}\""));
    }
    Ok(selected)
}

pub async fn load_child_templates(
    sessions: &Sessions,
    ids: &[AgentDefinitionId],
) -> Result<Vec<ChildTemplate>, String> {
    let mut templates = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            continue;
        }
        let definition = sessions
            .get_agent_definition(*id)
            .await
            .map_err(|error| error.to_string())?;
        let snapshot = sessions
            .get_resolved_agent_config(*id)
            .await
            .map_err(|error| error.to_string())?;
        let (models, default) = match snapshot {
            Some(snapshot) => (
                build_model_choices(snapshot.config.models),
                snapshot.config.default,
            ),
            None => (Vec::new(), true),
        };
        templates.push(ChildTemplate {
            definition_id: definition.id,
            name: definition.name,
            description: definition.description,
            models,
            default,
        });
    }
    Ok(templates)
}

fn build_model_choices(selections: Vec<ModelSelection>) -> Vec<ModelChoice> {
    let mut choices: Vec<ModelChoice> = Vec::new();
    for selection in selections {
        let mut label = model_label(&selection);
        // Distinct selections never share a label; suffix defensively anyway.
        let base = label.clone();
        let mut suffix = 2;
        while choices.iter().any(|choice| choice.label == label) {
            label = format!("{base}#{suffix}");
            suffix += 1;
        }
        choices.push(ModelChoice { label, selection });
    }
    choices
}

pub fn model_label(selection: &ModelSelection) -> String {
    match selection {
        ModelSelection::Global => "session".into(),
        ModelSelection::Parent => "parent".into(),
        ModelSelection::Fixed { model, effort } => match effort {
            EffortSelection::Parent => model.slug(),
            EffortSelection::Global => format!("{model}:session-effort"),
            EffortSelection::Fixed(effort) => format!("{model}:{}", effort.as_str()),
        },
    }
}

fn describe_model(selection: &ModelSelection) -> String {
    match selection {
        ModelSelection::Global => "the session's model".into(),
        ModelSelection::Parent => "same as yours".into(),
        ModelSelection::Fixed { model, effort } => match effort {
            EffortSelection::Parent => format!("{model}, your effort"),
            EffortSelection::Global => format!("{model}, the session's effort"),
            EffortSelection::Fixed(effort) => format!("{model}, {} effort", effort.as_str()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_api::providers::Effort;

    fn template(name: &str, models: Vec<ModelSelection>, default: bool) -> ChildTemplate {
        ChildTemplate {
            definition_id: AgentDefinitionId(1),
            name: name.into(),
            description: format!("{name} worker"),
            models: build_model_choices(models),
            default,
        }
    }

    fn fixed(effort: Effort) -> ModelSelection {
        ModelSelection::Fixed {
            model: "openrouter/vendor/model".parse().unwrap(),
            effort: EffortSelection::Fixed(effort),
        }
    }

    #[test]
    fn single_template_is_the_default_and_names_are_validated() {
        let only = [template("worker", vec![ModelSelection::Parent], true)];
        assert_eq!(select_template(&only, None).unwrap().name, "worker");
        let error = select_template(&only, Some("other")).unwrap_err();
        assert!(error.contains("Choose one of: worker"), "{error}");

        let two = [only[0].clone(), template("reviewer", vec![], true)];
        let error = select_template(&two, None).unwrap_err();
        assert!(error.contains("worker, reviewer"), "{error}");
        assert_eq!(
            select_template(&two, Some("reviewer")).unwrap().name,
            "reviewer"
        );
        assert!(select_template(&[], None).is_err());
    }

    #[test]
    fn model_labels_are_short_and_resolve_back_to_selections() {
        let worker = template(
            "worker",
            vec![ModelSelection::Parent, fixed(Effort::High)],
            false,
        );
        assert_eq!(worker.models[0].label, "parent");
        let fixed_label = &worker.models[1].label;
        assert_eq!(
            fixed_label,
            &format!("openrouter/vendor/model:{}", Effort::High.as_str())
        );
        assert_eq!(
            worker.resolve_model(Some(fixed_label)).unwrap(),
            Some(fixed(Effort::High))
        );
        assert!(
            worker
                .resolve_model(None)
                .unwrap_err()
                .contains("requires a model")
        );
        let error = worker.resolve_model(Some("nope")).unwrap_err();
        assert!(
            error.contains("Choose one of: parent, openrouter"),
            "{error}"
        );
    }

    #[test]
    fn defaults_and_fixed_models_need_no_choice() {
        let single = template("worker", vec![ModelSelection::Parent], true);
        assert_eq!(single.resolve_model(None).unwrap(), None);
        assert!(!single.has_model_choice());
        assert!(single.describe().contains("Model: same as yours."));

        let fixed_policy = template("worker", vec![], true);
        assert_eq!(fixed_policy.resolve_model(None).unwrap(), None);
        assert!(fixed_policy.resolve_model(Some("parent")).is_err());

        let defaulted = template(
            "worker",
            vec![ModelSelection::Global, ModelSelection::Parent],
            true,
        );
        assert_eq!(defaulted.resolve_model(None).unwrap(), None);
        assert!(
            defaulted
                .describe()
                .contains("\"session\" (the session's model, default), \"parent\" (same as yours)")
        );
    }
}
