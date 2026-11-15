//! Display text for decision-model calls, shared by every client UI.
use psa_api::{DecisionAnswerResult, DecisionCall, DecisionOutcome};
use std::time::Duration;

/// Multi-line description: the model, each headline answer, then usage.
pub fn describe_decision_call(call: &DecisionCall) -> String {
    let mut lines = vec![format!("Decision model {}", call.model.api_name)];

    match &call.outcome {
        DecisionOutcome::Failed(message) => lines.push(format!("failed: {message}")),
        DecisionOutcome::Answered | DecisionOutcome::PartlyAnswered => {
            lines.extend(call.answers.iter().map(|answer| {
                let confidence = answer
                    .confidence
                    .map(|confidence| format!(" (confidence {:.0}%)", confidence * 100.0))
                    .unwrap_or_default();
                format!(
                    "{}: {}{confidence}",
                    answer.key,
                    describe_result(&answer.result)
                )
            }));
        }
    }

    let mut usage = Vec::new();
    if let Some(tokens) = call.usage.input_tokens {
        usage.push(format!("{tokens} input tokens"));
    }
    if let Some(cost) = call.usage.cost {
        usage.push(format!("{cost} credits"));
    }
    usage.push(describe_duration(call.duration));
    lines.push(usage.join(" · "));

    lines.join("\n")
}

fn describe_result(result: &DecisionAnswerResult) -> String {
    match result {
        DecisionAnswerResult::Noul(probability) => format!("{:.0}% true", probability * 100.0),
        DecisionAnswerResult::Choice(choice) => choice.clone(),
        DecisionAnswerResult::Score(score) => format!("score {score:.2}"),
        DecisionAnswerResult::Refused => "refused".into(),
        DecisionAnswerResult::Unsupported => "unsupported answer".into(),
        DecisionAnswerResult::Missing => "no answer".into(),
    }
}

fn describe_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa_api::{DecisionAnswerSummary, DecisionCallUsage};

    fn call(outcome: DecisionOutcome, answers: Vec<DecisionAnswerSummary>) -> DecisionCall {
        DecisionCall {
            tool_call_id: None,
            model: llm_api::providers::Model::new(
                llm_api::providers::ProviderType::Openrouter,
                "vendor/decider",
            )
            .unwrap(),
            questions: Vec::new(),
            outcome,
            answers,
            usage: DecisionCallUsage {
                input_tokens: Some(128),
                cached_tokens: None,
                cost: Some(0.5),
            },
            duration: Duration::from_millis(1250),
            request_id: None,
        }
    }

    #[test]
    fn answers_usage_and_duration_are_listed() {
        let text = describe_decision_call(&call(
            DecisionOutcome::PartlyAnswered,
            vec![
                DecisionAnswerSummary {
                    key: "angry".into(),
                    result: DecisionAnswerResult::Noul(0.25),
                    confidence: Some(0.9),
                },
                DecisionAnswerSummary {
                    key: "action".into(),
                    result: DecisionAnswerResult::Choice("refund".into()),
                    confidence: None,
                },
                DecisionAnswerSummary {
                    key: "tone".into(),
                    result: DecisionAnswerResult::Refused,
                    confidence: None,
                },
            ],
        ));
        assert_eq!(
            text,
            "Decision model vendor/decider\nangry: 25% true (confidence 90%)\naction: refund\ntone: refused\n128 input tokens · 0.5 credits · 1.2s"
        );
    }

    #[test]
    fn failures_show_the_message_and_sub_second_durations() {
        let mut failed = call(
            DecisionOutcome::Failed("HTTP 429: slow down".into()),
            Vec::new(),
        );
        failed.duration = Duration::from_millis(40);
        failed.usage = DecisionCallUsage::default();
        assert_eq!(
            describe_decision_call(&failed),
            "Decision model vendor/decider\nfailed: HTTP 429: slow down\n40ms"
        );
    }
}
