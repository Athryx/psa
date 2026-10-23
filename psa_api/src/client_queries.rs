//! Ephemeral runtime requests for client input, shared by HTTP and event streams.
use crate::{AgentId, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientQueryId(pub uuid::Uuid);

impl ClientQueryId {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientQueryRequest {
    pub id: ClientQueryId,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub payload: ClientQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ClientQuery {
    AskQuestion(AskQuestionRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ClientQueryResponse {
    AskQuestion(AskQuestionResponse),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestionRequest {
    pub questions: Vec<ClientQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientQuestion {
    pub question: String,
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskQuestionResponse {
    pub answers: Vec<ClientAnswer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAnswer {
    pub answer: String,
    #[serde(default)]
    pub notes: Option<String>,
}

impl ClientQuery {
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

    /// Answers correspond to questions in order; choices are suggestions, not restrictions.
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
