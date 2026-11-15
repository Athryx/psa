//! `decide`: ask a decision model typed questions about text or JSON state.
//!
//! Decision models return probabilities in one request instead of generating
//! text, which makes them cheap enough to run over large amounts of data. Every
//! call is recorded as a public `DecisionCall` event.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use llm_api::{
    api::{
        HistoryItemKind, ToolSchema,
        decisions::{DecisionAnswer, DecisionQuestion, DecisionRequest, DecisionResponse},
    },
    providers::Model,
};
use psa_api::{
    DecisionAnswerResult, DecisionAnswerSummary, DecisionCall, DecisionCallUsage, DecisionOutcome,
    DecisionQuestionSummary,
    events::{EventType, NewEvent},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    CodeModeError, CodeModeFunction, CodeModeFunctionDefinition, CodeModeFunctionResult,
    typed_codemode_execute,
};
use crate::{
    agents::agent_context::AgentContext,
    components::{bundles::decision_models::DecisionAccess, cancellation::Cancellation},
};

pub struct Decide {
    access: DecisionAccess,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DecideArgs {
    /// Decision model from list_decision_models(). May be omitted when exactly
    /// one model is available or the session's decision model is allowed.
    #[serde(default)]
    pub(super) model: Option<String>,
    /// What to evaluate: a string, a JSON object, or a list of parts
    /// evaluated together.
    pub(super) state: Value,
    /// Your own key for each question, mapped to the question.
    pub(super) questions: BTreeMap<String, DecisionQuestion>,
}

impl Decide {
    pub(crate) fn new(access: DecisionAccess) -> Self {
        Self { access }
    }

    async fn execute_typed(
        &self,
        arguments: DecideArgs,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> CodeModeFunctionResult {
        let model = self.select_model(arguments.model.as_deref(), context)?;
        self.access
            .handle
            .describe(&model)
            .await
            .map_err(|message| value_error(&message))?;
        let request = DecisionRequest {
            model: model.clone(),
            state: arguments.state,
            questions: arguments.questions,
        };
        request
            .validate()
            .map_err(|error| value_error(&error.to_string()))?;

        let started = Instant::now();
        let result = self
            .access
            .handle
            .call(request.clone(), &mut cancellation)
            .await;
        let duration = started.elapsed();

        let call = match &result {
            Ok(response) => summarize_response(&model, &request, response, duration),
            Err(failure) if failure.cancelled => {
                return Err(CodeModeError::runtime(failure.message.clone()));
            }
            Err(failure) => summarize_failure(&model, &request, &failure.message, duration),
        };
        record_call(context, call).await;

        let response = result.map_err(|failure| CodeModeError::runtime(failure.message))?;
        Ok(answers_value(&model, &request, response))
    }

    /// An explicit name must be legal; otherwise the policy's default is used.
    fn select_model(
        &self,
        requested: Option<&str>,
        context: &AgentContext<'_>,
    ) -> Result<Model, CodeModeError> {
        let policy = &self.access.policy;
        let legal = policy.legal_models(context.session());
        let listing = || legal.iter().map(Model::slug).collect::<Vec<_>>().join(", ");

        let Some(requested) = requested else {
            if let Some(model) = policy.default_model(context.session()) {
                return Ok(model);
            }
            if legal.is_empty() {
                return Err(value_error(
                    "No decision models are available to this agent",
                ));
            }
            return Err(value_error(&format!(
                "model is required; choose one of: {}",
                listing()
            )));
        };
        let model =
            Model::parse_slug(requested).map_err(|error| value_error(&error.to_string()))?;
        if legal.contains(&model) {
            return Ok(model);
        }
        Err(value_error(&format!(
            "{requested} is not an allowed decision model; choose one of: {}",
            listing()
        )))
    }
}

fn value_error(message: &str) -> CodeModeError {
    CodeModeError {
        exception_type: "ValueError".into(),
        message: message.into(),
    }
}

fn question_summaries(request: &DecisionRequest) -> Vec<DecisionQuestionSummary> {
    request
        .questions
        .iter()
        .map(|(key, question)| DecisionQuestionSummary {
            key: key.clone(),
            kind: question.kind().into(),
        })
        .collect()
}

fn summarize_answer(answer: Option<&DecisionAnswer>) -> (DecisionAnswerResult, Option<f64>) {
    match answer {
        Some(DecisionAnswer::Noul { noul, confidence }) => {
            (DecisionAnswerResult::Noul(*noul), *confidence)
        }
        Some(DecisionAnswer::Choice {
            choice, confidence, ..
        }) => (DecisionAnswerResult::Choice(choice.clone()), *confidence),
        Some(DecisionAnswer::Score {
            score, confidence, ..
        }) => (DecisionAnswerResult::Score(*score), *confidence),
        Some(DecisionAnswer::Refusal) => (DecisionAnswerResult::Refused, None),
        Some(DecisionAnswer::Unsupported) => (DecisionAnswerResult::Unsupported, None),
        None => (DecisionAnswerResult::Missing, None),
    }
}

fn summarize_response(
    model: &Model,
    request: &DecisionRequest,
    response: &DecisionResponse,
    duration: Duration,
) -> DecisionCall {
    let answers: Vec<_> = request
        .questions
        .keys()
        .map(|key| {
            let (result, confidence) = summarize_answer(response.answers.get(key));
            DecisionAnswerSummary {
                key: key.clone(),
                result,
                confidence,
            }
        })
        .collect();
    let complete = answers.iter().all(|answer| {
        !matches!(
            answer.result,
            DecisionAnswerResult::Refused
                | DecisionAnswerResult::Unsupported
                | DecisionAnswerResult::Missing
        )
    });
    DecisionCall {
        tool_call_id: None,
        model: model.clone(),
        questions: question_summaries(request),
        outcome: if complete {
            DecisionOutcome::Answered
        } else {
            DecisionOutcome::PartlyAnswered
        },
        answers,
        usage: DecisionCallUsage {
            input_tokens: response.usage.input_tokens,
            cached_tokens: response.usage.cached_input_tokens,
            cost: response.usage.cost,
        },
        duration,
        request_id: response.response_id.clone(),
    }
}

fn summarize_failure(
    model: &Model,
    request: &DecisionRequest,
    message: &str,
    duration: Duration,
) -> DecisionCall {
    DecisionCall {
        tool_call_id: None,
        model: model.clone(),
        questions: question_summaries(request),
        outcome: DecisionOutcome::Failed(message.into()),
        answers: Vec::new(),
        usage: DecisionCallUsage::default(),
        duration,
        request_id: None,
    }
}

/// Unanswered questions are `None` to the script: a refusal or a missing
/// answer is not a negative verdict and must not be read as one.
fn answers_value(model: &Model, request: &DecisionRequest, response: DecisionResponse) -> Value {
    let answers: serde_json::Map<String, Value> = request
        .questions
        .keys()
        .map(|key| {
            let value = match response.answers.get(key) {
                Some(DecisionAnswer::Refusal | DecisionAnswer::Unsupported) | None => Value::Null,
                Some(answer) => serde_json::to_value(answer).unwrap_or(Value::Null),
            };
            (key.clone(), value)
        })
        .collect();
    json!({
        "model": model.slug(),
        "cost": response.usage.cost,
        "answers": answers,
    })
}

async fn record_call(context: &mut AgentContext<'_>, mut call: DecisionCall) {
    call.tool_call_id = current_codemode_call_id(context);
    let saved = context
        .save_events(&[NewEvent::new(EventType::DecisionCall(call))])
        .await;
    // The provider has already answered and charged; losing the record must not
    // lose the result.
    if let Err(error) = saved {
        tracing::warn!(%error, "could not record decision call");
    }
}

/// The CodeMode tool call that is currently executing: the newest `codemode`
/// call without a result. CodeMode runs sequentially, so there is at most one.
fn current_codemode_call_id(context: &AgentContext<'_>) -> Option<String> {
    let mut resolved = std::collections::HashSet::new();
    for item in context.projection().items().iter().rev() {
        match &item.kind {
            HistoryItemKind::ToolResult(result) => {
                resolved.insert(result.call_id.as_str());
            }
            HistoryItemKind::ToolCall(call)
                if call.name == "codemode" && !resolved.contains(call.call_id.as_str()) =>
            {
                return Some(call.call_id.clone());
            }
            _ => {}
        }
    }
    None
}

impl CodeModeFunction for Decide {
    fn definition(&self) -> CodeModeFunctionDefinition {
        CodeModeFunctionDefinition {
            name: "decide".into(),
            description: "Ask a decision model typed questions about state (a string, a JSON object, or a list of parts) and get probabilities back in one cheap request; it generates no text. Use it to classify, filter or score large amounts of data, and run many calls concurrently with asyncio.gather. questions maps your own key to a question: {type: \"noul\", instructions} is yes/no; {type: \"choice\", instructions, criteria: {option: description}} picks one option; {type: \"score\", instructions, criteria: [lowest, ..., highest]} rates on a scale. Returns {model, cost, answers} where answers maps each key to {type, noul | choice | score, confidence, probabilities}, or None when the model refused or did not answer; None is not a negative verdict. See list_decision_models() for models.".into(),
            parameters: ToolSchema::for_type::<DecideArgs>(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_codemode_execute!(DecideArgs, execute_typed);
}
