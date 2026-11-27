//! Client for decision-model APIs ("System One" models).
//!
//! Decision models do not generate text. They answer typed questions about a
//! state and return probabilities, so they use their own request/response
//! shape instead of an `AgentHistory`. A request is a single non-streaming
//! HTTP call; dropping the future cancels it. Calls are never retried because
//! a retry could be billed twice.
//!
//! The wire format follows OpenRouter's alpha Decisions API. State is passed
//! through as arbitrary JSON. Only the `noul`, `choice` and `score` question
//! types are modeled; other answer types decode as
//! [`DecisionAnswer::Unsupported`].

use std::collections::BTreeMap;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::agent_history::require_provider;
use super::transport::{build_authenticated_headers, http_error, parse_endpoint_url, parse_usage};
use super::{ProviderFields, Usage};
use crate::Error;
use crate::providers::{Model, ProviderType};

/// Maximum accepted response body; decision answers are small.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Binary question outcome descriptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoulCriteria {
    /// Meaning of a true answer.
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub when_true: Option<String>,
    /// Meaning of a false answer.
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub when_false: Option<String>,
}

/// One typed question. The key naming it lives in the request's map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum DecisionQuestion {
    /// Binary question answered with a probability of true.
    Noul {
        /// What to decide.
        instructions: String,
        /// Optional descriptions of the true and false outcomes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Multi-class question answered with the winning option.
    Choice {
        /// What to decide.
        instructions: String,
        /// Option key to option description; at least two options.
        criteria: BTreeMap<String, String>,
    },
    /// Ordered-scale question answered with a continuous score.
    Score {
        /// What to decide.
        instructions: String,
        /// Bucket descriptions ordered from minimum to maximum; at least two.
        criteria: Vec<String>,
    },
}

impl DecisionQuestion {
    /// The wire name of this question type.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }

    fn validate(&self, key: &str) -> Result<(), Error> {
        let (instructions, valid) = match self {
            Self::Noul { instructions, .. } => (instructions, true),
            Self::Choice {
                instructions,
                criteria,
            } => (instructions, criteria.len() >= 2),
            Self::Score {
                instructions,
                criteria,
            } => (instructions, criteria.len() >= 2),
        };
        if instructions.trim().is_empty() {
            return Err(invalid(format!(
                "question {key:?} needs nonempty instructions"
            )));
        }
        if !valid {
            return Err(invalid(format!(
                "question {key:?} needs at least two criteria"
            )));
        }
        Ok(())
    }
}

/// A decision request, such as for `openrouter`'s `typesafe/jev-1.14`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    /// Decision model; its provider must match the client's.
    pub model: Model,
    /// Context the questions are evaluated against: a string, a JSON object,
    /// or an array of parts, which may include media parts such as `image_url`.
    pub state: serde_json::Value,
    /// Question key to question; keys are echoed in the response.
    pub questions: BTreeMap<String, DecisionQuestion>,
}

impl DecisionRequest {
    /// Local checks only; the provider may still reject the request.
    pub fn validate(&self) -> Result<(), Error> {
        self.model.validate()?;
        if self.questions.is_empty() {
            return Err(invalid("at least one question is required".into()));
        }
        for (key, question) in &self.questions {
            if key.trim().is_empty() {
                return Err(invalid("question keys must be nonempty".into()));
            }
            question.validate(key)?;
        }
        Ok(())
    }
}

/// One answer, matching its question's type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionAnswer {
    /// Probability that the binary question is true, from 0 to 1.
    Noul {
        /// Probability of true.
        noul: f64,
        /// Model confidence in its answer, when reported.
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// Winning option with the distribution over all options.
    Choice {
        /// Key of the most probable option.
        choice: String,
        /// Option key to probability; sums to one.
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        /// Model confidence in its answer, when reported.
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// Continuous position on the ordered scale, from 0 to the bucket count minus one.
    Score {
        /// Score on the scale.
        score: f64,
        /// Bucket index to probability.
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        /// Bucket index to the criterion it describes, when reported.
        #[serde(default)]
        legend: BTreeMap<String, serde_json::Value>,
        /// Model confidence in its answer, when reported.
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// The provider declined this question; other questions still resolve.
    Refusal,
    /// An answer type this client does not model, such as a bounding box.
    #[serde(other)]
    Unsupported,
}

impl DecisionAnswer {
    /// False for a refusal or an answer type this client does not model.
    pub fn is_answered(&self) -> bool {
        !matches!(self, Self::Refusal | Self::Unsupported)
    }

    /// Model confidence in the answer, when reported.
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Self::Noul { confidence, .. }
            | Self::Choice { confidence, .. }
            | Self::Score { confidence, .. } => *confidence,
            Self::Refusal | Self::Unsupported => None,
        }
    }
}

/// A completed decision call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionResponse {
    /// Model the request was sent to.
    pub model: Model,
    /// Provider request identifier.
    pub response_id: Option<String>,
    /// Answers keyed by question key. A requested key may be absent; callers
    /// must treat that as unanswered, never as a negative verdict.
    pub answers: BTreeMap<String, DecisionAnswer>,
    /// Token counts; the provider's raw usage, including `cost`, is kept in
    /// `provider_details`.
    pub usage: Usage,
    /// Other response fields, such as the served model version and upstream provider.
    pub provider_metadata: ProviderFields,
}

impl DecisionResponse {
    /// True when every question in the request received a modeled answer.
    pub fn is_complete(&self, request: &DecisionRequest) -> bool {
        request.questions.keys().all(|key| {
            self.answers
                .get(key)
                .is_some_and(DecisionAnswer::is_answered)
        })
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    state: &'a serde_json::Value,
    questions: &'a BTreeMap<String, DecisionQuestion>,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    id: Option<String>,
    answers: BTreeMap<String, DecisionAnswer>,
    usage: serde_json::Value,
    #[serde(flatten)]
    extra: ProviderFields,
}

/// Configured decision-model client.
#[derive(Clone)]
pub struct Decisions {
    client: reqwest::Client,
    url: reqwest::Url,
    provider: ProviderType,
}

impl Decisions {
    /// Use the provider's standard decision endpoint, or fail when it has none.
    pub fn new(provider: ProviderType, api_key: impl Into<super::Auth>) -> Result<Self, Error> {
        let url = provider.decisions_url().ok_or_else(|| Error::Unsupported {
            feature: format!("decision models on {}", provider.name()),
        })?;
        Self::with_url(provider, url, api_key)
    }

    /// The URL is the complete endpoint; no path is appended.
    pub fn with_url(
        provider: ProviderType,
        url: &str,
        api_key: impl Into<super::Auth>,
    ) -> Result<Self, Error> {
        let url = parse_endpoint_url(url)?;
        let mut headers = build_authenticated_headers(&api_key.into())?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            // Do not forward secrets or silently change request semantics on redirects.
            .redirect(reqwest::redirect::Policy::none())
            // A retry could create a second billable request.
            .retry(reqwest::retry::never())
            .build()?;

        Ok(Self {
            client,
            url,
            provider,
        })
    }

    /// Provider identity this client sends requests to.
    pub fn provider_type(&self) -> ProviderType {
        self.provider
    }

    /// Send one decision request and read its complete response.
    pub async fn decide(&self, request: &DecisionRequest) -> Result<DecisionResponse, Error> {
        require_provider(self.provider, request.model.provider)?;
        request.validate()?;
        let wire = WireRequest {
            model: &request.model.api_name,
            state: &request.state,
            questions: &request.questions,
        };

        let mut response = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .json(&wire)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::Protocol {
                    message: "decision response exceeds the size limit".into(),
                });
            }
            body.extend_from_slice(&chunk);
        }

        parse_response(request, &body)
    }
}

fn parse_response(request: &DecisionRequest, body: &[u8]) -> Result<DecisionResponse, Error> {
    let wire: WireResponse = serde_json::from_slice(body)?;

    Ok(DecisionResponse {
        model: request.model.clone(),
        response_id: wire.id,
        answers: wire.answers,
        usage: parse_usage(&wire.usage, false),
        provider_metadata: wire.extra,
    })
}

fn invalid(message: String) -> Error {
    Error::InvalidRequest { message }
}
