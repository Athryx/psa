//! Transcript record of a decision-model call.
//!
//! Decision models answer typed questions about a state and return
//! probabilities instead of text. The event keeps the complete request,
//! including its state, and the provider's response or failure.
use llm_api::{
    api::{
        Usage,
        decisions::{DecisionRequest, DecisionResponse},
    },
    providers::Model,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One decision-model call made by an agent, usually from CodeMode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionCall {
    /// CodeMode tool call that made this call, when it could be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Request as sent, including the evaluated state.
    pub request: DecisionRequest,
    /// Provider response, or why the call failed before any answer arrived.
    pub response: Result<DecisionResponse, String>,
    /// Wall-clock time of the request.
    pub duration: Duration,
}

impl DecisionCall {
    /// Decision model that was called.
    pub fn model(&self) -> &Model {
        &self.request.model
    }

    /// Reported usage; `None` when the call failed and nothing was charged.
    pub fn usage(&self) -> Option<&Usage> {
        self.response.as_ref().ok().map(|response| &response.usage)
    }
}
