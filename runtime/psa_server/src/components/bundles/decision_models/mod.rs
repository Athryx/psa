//! CodeMode access to decision models, limited by the agent definition.
//!
//! Decision models answer typed questions with probabilities in one request
//! and are called only from CodeMode, so this bundle installs functions and no
//! model-facing tools. The legal set is the definition's ordered selections of
//! fixed models, the session's decision model, and the catalog's pinned
//! decision models, resolved at each call so it follows session and pin changes.

mod handle;

pub(crate) use handle::{DEFAULT_MAX_CONCURRENT_CALLS, DecisionHandle};

use super::{Bundle, CleanupError, InstallError, Installer, session::SessionBundleHandle};
use crate::{
    agents::Agent,
    components::codemode_functions::{
        CodeModeFunctionKind, decide::Decide, list_decision_models::ListDecisionModels,
    },
    sessions::Session,
};
use llm_api::providers::Model;
use psa::api::DecisionModelSelection;
use std::sync::Arc;

/// Which models an agent may call, resolved against its session and the
/// catalog's pinned models at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionPolicy {
    models: Vec<DecisionModelSelection>,
    first_model_default: bool,
}

impl DecisionPolicy {
    pub(crate) fn new(models: &[DecisionModelSelection], first_model_default: bool) -> Self {
        Self {
            models: models.to_vec(),
            first_model_default,
        }
    }

    /// The selections expanded in order, keeping the first of repeated models.
    /// `pinned` is called once, at the first `Pinned` selection; later ones
    /// could only repeat its models.
    pub(crate) async fn legal_models(
        &self,
        session: &Session,
        pinned: impl AsyncFnOnce() -> Result<Vec<Model>, String>,
    ) -> Result<Vec<Model>, String> {
        let mut pinned = Some(pinned);
        let mut models = Vec::new();
        for selection in &self.models {
            let expanded = match selection {
                DecisionModelSelection::Fixed(model) => vec![model.clone()],
                DecisionModelSelection::Global => session.decision_model.iter().cloned().collect(),
                DecisionModelSelection::Pinned => match pinned.take() {
                    Some(load) => load().await?,
                    None => Vec::new(),
                },
            };
            for model in expanded {
                if !models.contains(&model) {
                    models.push(model);
                }
            }
        }
        Ok(models)
    }

    /// The model used when a call names none: the first legal model when the
    /// policy makes it the default.
    pub(crate) fn default_model(&self, legal: &[Model]) -> Option<Model> {
        legal.first().filter(|_| self.first_model_default).cloned()
    }
}

/// Everything a decision function needs: what is allowed and how to call it.
#[derive(Clone)]
pub(crate) struct DecisionAccess {
    pub(crate) policy: Arc<DecisionPolicy>,
    pub(crate) handle: DecisionHandle,
}

impl DecisionAccess {
    /// The models this agent may call right now, in policy order.
    pub(crate) async fn legal_models(&self, session: &Session) -> Result<Vec<Model>, String> {
        self.policy
            .legal_models(session, async || self.handle.pinned_models().await)
            .await
    }
}

pub struct DecisionModelsBundle {
    access: DecisionAccess,
}

impl DecisionModelsBundle {
    pub fn new(
        models: &[DecisionModelSelection],
        first_model_default: bool,
        resources: &SessionBundleHandle,
    ) -> Self {
        Self {
            access: DecisionAccess {
                policy: Arc::new(DecisionPolicy::new(models, first_model_default)),
                handle: resources.decisions(),
            },
        }
    }
}

impl Bundle for DecisionModelsBundle {
    fn install(
        &mut self,
        _agent: &Agent,
        _session: &Session,
        installer: &mut Installer,
    ) -> Result<(), InstallError> {
        installer.add_help_only_codemode_function(CodeModeFunctionKind::ListDecisionModels(
            ListDecisionModels::new(self.access.clone()),
        ))?;
        installer.add_help_only_codemode_function(CodeModeFunctionKind::Decide(Decide::new(
            self.access.clone(),
        )))?;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), CleanupError> {
        Ok(())
    }
}
