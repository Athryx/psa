//! Transcript record of a decision-model call.
//!
//! Decision models answer typed questions about a state and return
//! probabilities instead of text. The event records what was asked and the
//! headline answers, never the state itself, which may be large or sensitive.
use llm_api::providers::Model;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Question that was asked, without its instructions or criteria.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionQuestionSummary {
    /// Key naming the question in the request.
    pub key: String,
    /// Question type: `noul`, `choice` or `score`.
    pub kind: String,
}

/// Headline result of one answered question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DecisionAnswerResult {
    /// Probability that a binary question is true, from 0 to 1.
    Noul(#[doc = "Probability of true."] f64),
    /// Key of the winning option.
    Choice(#[doc = "Winning option key."] String),
    /// Position on the ordered scale.
    Score(#[doc = "Continuous score."] f64),
    /// The provider declined this question.
    Refused,
    /// The provider returned an answer type this server cannot represent.
    Unsupported,
    /// The provider omitted an answer for a requested question.
    Missing,
}

/// One question's headline answer; probability distributions are not recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAnswerSummary {
    /// Key naming the question in the request.
    pub key: String,
    /// Headline result.
    pub result: DecisionAnswerResult,
    /// Model confidence, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// How a decision call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionOutcome {
    /// Every requested question received an answer.
    Answered,
    /// At least one question was refused, unsupported or missing.
    PartlyAnswered,
    /// The call failed or was cancelled before any answer arrived.
    Failed(#[doc = "Failure description."] String),
}

/// Usage and charge reported for a call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionCallUsage {
    /// Input tokens; `None` means unreported.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Input tokens served from cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Credits charged by the provider, when reported.
    #[serde(default)]
    pub cost: Option<f64>,
}

/// One decision-model call made by an agent, usually from CodeMode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionCall {
    /// CodeMode tool call that made this call, when it could be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Decision model that was called.
    pub model: Model,
    /// Questions in request order by key.
    pub questions: Vec<DecisionQuestionSummary>,
    /// How the call ended.
    pub outcome: DecisionOutcome,
    /// Headline answers; empty when the call failed.
    pub answers: Vec<DecisionAnswerSummary>,
    /// Usage and charge.
    pub usage: DecisionCallUsage,
    /// Wall-clock time of the request.
    pub duration: Duration,
    /// Provider request identifier, when returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}
