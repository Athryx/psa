//! Ephemeral runtime requests for client input, shared by HTTP and event streams.
use crate::{AgentId, SessionId};
use serde::{Deserialize, Serialize};

/// Unique identifier for a pending runtime client query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientQueryId(
    /// UUID used to correlate a query with its response.
    pub uuid::Uuid,
);

impl ClientQueryId {
    /// Generate a random version-4 query identifier.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for ClientQueryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ClientQueryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Pending client input requested by an agent; not persisted across server restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientQueryRequest {
    /// Identifier to use when submitting a response.
    pub id: ClientQueryId,
    /// Session containing the requesting agent.
    pub session_id: SessionId,
    /// Agent waiting for client input.
    pub agent_id: AgentId,
    /// Input requested from the client.
    pub payload: ClientQuery,
}

/// Typed request for interactive input from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ClientQuery {
    /// Ask the client to answer one or more questions.
    AskQuestion(
        /// Questions and optional suggested choices.
        AskQuestionRequest,
    ),
}

/// Typed client response to a pending query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ClientQueryResponse {
    /// Answers to an ask-question request.
    AskQuestion(
        /// Answers in the same order as the requested questions.
        AskQuestionResponse,
    ),
}

/// Nonempty ordered list of questions to present to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskQuestionRequest {
    /// Questions whose order determines the response answer order.
    pub questions: Vec<ClientQuestion>,
}

/// A question with optional suggested answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientQuestion {
    /// Nonblank text to present to the client.
    pub question: String,
    /// Nonblank suggestions; free-form answers are still accepted.
    #[serde(default)]
    pub choices: Vec<String>,
}

/// Answers corresponding one-to-one with the requested questions, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestionResponse {
    /// One answer for each question in the request.
    pub answers: Vec<ClientAnswer>,
}

/// A client's answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAnswer {
    /// Nonblank answer text, not limited to the suggested choices.
    pub answer: String,
    /// Optional supplemental explanation from the client.
    #[serde(default)]
    pub notes: Option<String>,
}

impl ClientQuery {
    /// Reject empty question lists, blank questions, or blank suggested choices.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::AskQuestion(request) => {
                if request.questions.is_empty() {
                    return Err("questions must not be empty".into());
                }
                for question in &request.questions {
                    if question.question.trim().is_empty() {
                        return Err("question must not be blank".into());
                    }
                    if question
                        .choices
                        .iter()
                        .any(|choice| choice.trim().is_empty())
                    {
                        return Err("choices must not be blank".into());
                    }
                }
                Ok(())
            }
        }
    }

    /// Require one nonblank answer per question, in order; choices are suggestions, not restrictions.
    pub fn validate_response(&self, response: &ClientQueryResponse) -> Result<(), String> {
        match (self, response) {
            (Self::AskQuestion(request), ClientQueryResponse::AskQuestion(response)) => {
                if response.answers.len() != request.questions.len() {
                    return Err("answer count must match question count".into());
                }
                if response
                    .answers
                    .iter()
                    .any(|answer| answer.answer.trim().is_empty())
                {
                    return Err("answers must not be blank".into());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn question_request_schema_preserves_the_closed_wire_contract() {
        let schema = serde_json::to_value(schemars::schema_for!(AskQuestionRequest)).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["questions"]));
        assert_eq!(schema["properties"]["questions"]["type"], "array");

        let question = serde_json::to_value(schemars::schema_for!(ClientQuestion)).unwrap();
        assert_eq!(question["additionalProperties"], false);
        assert_eq!(question["required"], json!(["question"]));
        assert_eq!(question["properties"]["question"]["type"], "string");
        assert_eq!(question["properties"]["choices"]["type"], "array");
        assert_eq!(question["properties"]["choices"]["default"], json!([]));

        let request: AskQuestionRequest = serde_json::from_value(json!({
            "questions": [{"question": "Which option?"}],
        }))
        .unwrap();
        assert!(request.questions[0].choices.is_empty());
        for invalid in [
            json!({"questions": [], "unknown": true}),
            json!({"questions": [{"question": "Which option?", "unknown": true}]}),
        ] {
            assert!(serde_json::from_value::<AskQuestionRequest>(invalid).is_err());
        }
    }
}
