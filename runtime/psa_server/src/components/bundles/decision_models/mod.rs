//! CodeMode access to decision models, limited by the agent definition.
//!
//! Decision models answer typed questions with probabilities in one request
//! and are called only from CodeMode, so this bundle installs functions and no
//! model-facing tools. The legal set is the definition's own models plus,
//! optionally, the session's decision model, which follows session updates.

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
use std::sync::Arc;

/// Which models an agent may call, resolved against its session at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionPolicy {
    include_global: bool,
    models: Vec<Model>,
}

impl DecisionPolicy {
    /// Policy over the configured models, dropping repeats.
    pub(crate) fn new(include_global: bool, configured: &[Model]) -> Self {
        let mut models = Vec::new();
        for model in configured {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        Self {
            include_global,
            models,
        }
    }

    /// Configured models first, then the session's decision model when included.
    pub(crate) fn legal_models(&self, session: &Session) -> Vec<Model> {
        let mut models = self.models.clone();
        let global = session
            .decision_model
            .as_ref()
            .filter(|_| self.include_global);
        if let Some(global) = global {
            if !models.contains(global) {
                models.push(global.clone());
            }
        }
        models
    }

    /// The model used when a call names none: the session's decision model
    /// when included, otherwise the only configured model.
    pub(crate) fn default_model(&self, session: &Session) -> Option<Model> {
        if self.include_global
            && let Some(global) = &session.decision_model
        {
            return Some(global.clone());
        }
        match self.legal_models(session).as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }
}

/// Everything a decision function needs: what is allowed and how to call it.
#[derive(Clone)]
pub(crate) struct DecisionAccess {
    pub(crate) policy: Arc<DecisionPolicy>,
    pub(crate) handle: DecisionHandle,
}

pub struct DecisionModelsBundle {
    access: DecisionAccess,
}

impl DecisionModelsBundle {
    pub fn new(include_global: bool, models: &[Model], resources: &SessionBundleHandle) -> Self {
        Self {
            access: DecisionAccess {
                policy: Arc::new(DecisionPolicy::new(include_global, models)),
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
