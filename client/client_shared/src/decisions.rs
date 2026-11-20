//! Display text for decision-model calls, shared by every client UI.
use llm_api::api::decisions::DecisionAnswer;
use psa::api::DecisionCall;
use std::time::Duration;

/// Multi-line description: the model, each question's headline answer in
/// key order, then usage.
pub fn describe_decision_call(call: &DecisionCall) -> String {
    let mut lines = vec![format!("Decision model {}", call.model().api_name)];

    match &call.response {
        Err(message) => lines.push(format!("failed: {message}")),
        Ok(response) => {
            lines.extend(call.request.questions.keys().map(|key| {
                let answer = response.answers.get(key);
                let confidence = answer
                    .and_then(DecisionAnswer::confidence)
                    .map(|confidence| format!(" (confidence {:.0}%)", confidence * 100.0))
                    .unwrap_or_default();
                format!("{key}: {}{confidence}", describe_answer(answer))
            }));
        }
    }

    let mut usage = Vec::new();
    if let Some(tokens) = call.usage().and_then(|usage| usage.input_tokens) {
        usage.push(format!("{tokens} input tokens"));
    }
    if let Some(cost) = call.usage().and_then(|usage| usage.cost) {
        usage.push(format!("{cost} credits"));
    }
    usage.push(describe_duration(call.duration));
    lines.push(usage.join(" · "));

    lines.join("\n")
}

/// One-line summary for a collapsed row: the model, how the call ended, and
/// how long it took.
pub fn summarize_decision_call(call: &DecisionCall) -> String {
    let outcome = match &call.response {
        Err(message) => format!("failed: {}", message.lines().next().unwrap_or_default()),
        Ok(response) => match response.answers.len() {
            1 => "1 answer".into(),
            count => format!("{count} answers"),
        },
    };
    format!(
        "{} · {outcome} · {}",
        call.model().api_name,
        describe_duration(call.duration)
    )
}

/// Headline result only; probability distributions are left to the event.
fn describe_answer(answer: Option<&DecisionAnswer>) -> String {
    match answer {
        Some(DecisionAnswer::Noul { noul, .. }) => format!("{:.0}% true", noul * 100.0),
        Some(DecisionAnswer::Choice { choice, .. }) => choice.clone(),
        Some(DecisionAnswer::Score { score, .. }) => format!("score {score:.2}"),
        Some(DecisionAnswer::Refusal) => "refused".into(),
        Some(DecisionAnswer::Unsupported) => "unsupported answer".into(),
        None => "no answer".into(),
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
    use llm_api::{
        api::{
            Usage,
            decisions::{DecisionQuestion, DecisionRequest, DecisionResponse},
        },
        providers::Model,
    };
    use std::collections::BTreeMap;

    fn noul(instructions: &str) -> DecisionQuestion {
        DecisionQuestion::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    fn call(response: Result<DecisionResponse, String>) -> DecisionCall {
        let model: Model = "openrouter/vendor/decider".parse().unwrap();
        DecisionCall {
            tool_call_id: None,
            request: DecisionRequest {
                model,
                state: "ticket".into(),
                questions: BTreeMap::from([
                    ("angry".into(), noul("Is the customer angry?")),
                    (
                        "action".into(),
                        DecisionQuestion::Choice {
                            instructions: "What should happen?".into(),
                            criteria: BTreeMap::from([
                                ("refund".into(), "Refund".into()),
                                ("reply".into(), "Reply".into()),
                            ]),
                        },
                    ),
                    ("tone".into(), noul("Is the tone polite?")),
                    ("urgent".into(), noul("Is it urgent?")),
                ]),
            },
            response,
            duration: Duration::from_millis(1250),
        }
    }

    fn response(answers: BTreeMap<String, DecisionAnswer>) -> DecisionResponse {
        DecisionResponse {
            model: "openrouter/vendor/decider".parse().unwrap(),
            response_id: None,
            answers,
            usage: Usage {
                input_tokens: Some(128),
                cost: Some(0.5),
                ..Usage::default()
            },
            provider_metadata: Default::default(),
        }
    }

    #[test]
    fn answers_usage_and_duration_are_listed() {
        let text = describe_decision_call(&call(Ok(response(BTreeMap::from([
            (
                "angry".into(),
                DecisionAnswer::Noul {
                    noul: 0.25,
                    confidence: Some(0.9),
                },
            ),
            (
                "action".into(),
                DecisionAnswer::Choice {
                    choice: "refund".into(),
                    probabilities: BTreeMap::new(),
                    confidence: None,
                },
            ),
            ("tone".into(), DecisionAnswer::Refusal),
        ])))));
        assert_eq!(
            text,
            "Decision model vendor/decider\naction: refund\nangry: 25% true (confidence 90%)\ntone: refused\nurgent: no answer\n128 input tokens · 0.5 credits · 1.2s"
        );
    }

    #[test]
    fn failures_show_the_message_and_sub_second_durations() {
        let mut failed = call(Err("HTTP 429: slow down".into()));
        failed.duration = Duration::from_millis(40);
        assert_eq!(
            describe_decision_call(&failed),
            "Decision model vendor/decider\nfailed: HTTP 429: slow down\n40ms"
        );
        assert_eq!(
            summarize_decision_call(&failed),
            "vendor/decider · failed: HTTP 429: slow down · 40ms"
        );
    }
}
