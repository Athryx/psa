//! End-to-end adapter tests against a local HTTP server; no API credentials needed.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::*;
use crate::providers::{Effort, Model, ModelConfig, ProviderType};

struct RecordedRequest {
    headers: String,
    body: Value,
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> RecordedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];

    let (header_end, content_length) = loop {
        let size = socket.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..size]);

        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&bytes[..end]);
            let length = text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };

    while bytes.len() < header_end + content_length {
        let size = socket.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0, "request ended before body");
        bytes.extend_from_slice(&chunk[..size]);
    }

    RecordedRequest {
        headers: String::from_utf8(bytes[..header_end].to_vec()).unwrap(),
        body: serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap(),
    }
}

async fn server(
    status: &str,
    content_type: &str,
    extra_headers: &str,
    body: String,
) -> (String, oneshot::Receiver<RecordedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    );
    let (sender, receiver) = oneshot::channel();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = sender.send(read_request(&mut socket).await);
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }

        // Split UTF-8, SSE fields, and JSON across writes deliberately.
        for fragment in body.as_bytes().chunks(7) {
            if socket.write_all(fragment).await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
    });

    (format!("http://{address}/custom-endpoint"), receiver)
}

fn history(provider: ProviderType) -> AgentHistory {
    AgentHistory::new(ModelConfig {
        provider,
        model: Model::new("test-model").unwrap(),
        effort: Effort::Medium,
    })
}

fn input() -> ModelTurnInput {
    ModelTurnInput {
        items: vec![HistoryItem {
            kind: HistoryItemKind::Message(Message {
                role: Role::User,
                content: vec![ContentBlock::Text(TextBlock {
                    text: "Hi".into(),
                    annotations: Vec::new(),
                })],
            }),
            metadata: HistoryItemMetadata::default(),
        }],
    }
}

fn text(output: &ModelTurnOutput) -> String {
    output
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            HistoryItemKind::Message(message) => Some(&message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn chat_stream(done: bool) -> String {
    let chunks = [
        json!({"id":"chat-1","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
        json!({"id":"chat-1","choices":[{"index":0,"delta":{"content":"héllo"},"finish_reason":null}]}),
        json!({"id":"chat-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"chat-1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
    ];

    let mut stream = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>();

    if done {
        stream.push_str("data: [DONE]\n\n");
    }

    stream
}

fn responses_stream() -> String {
    let part = json!({"type":"output_text","text":"héllo","annotations":[]});
    let item = json!({"id":"msg-1","type":"message","role":"assistant","status":"completed","content":[part.clone()]});
    let events = [
        json!({"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg-1","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"item_id":"msg-1","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg-1","content_index":0,"delta":"héllo"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"msg-1","content_index":0,"text":"héllo"}),
        json!({"type":"response.content_part.done","output_index":0,"item_id":"msg-1","content_index":0,"part":part}),
        json!({"type":"response.output_item.done","output_index":0,"item":item.clone()}),
        json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[item],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}),
    ];

    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\r\ndata: {event}\r\n\r\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

// Public variants also permit custom endpoints for proxies/testing.
fn provider_at_url(provider: ProviderType, url: String) -> Provider {
    match provider {
        ProviderType::OpenAiCodex => {
            Provider::Responses(OpenAiResponses::new(url, "key", provider).unwrap())
        }
        ProviderType::Openrouter => Provider::Completions(
            OpenAiCompletions::new(
                url,
                "key",
                provider,
                OpenAiCompletionsConfig {
                    openrouter_reasoning_fields: true,
                },
            )
            .unwrap(),
        ),
    }
}

async fn check_completed_stream(mut stream: ProviderStream) {
    let mut finished = 0;
    while let Some(event) = stream.next_event().await {
        if matches!(event.unwrap(), TurnEvent::Finished) {
            finished += 1;
        }
    }

    assert_eq!(finished, 1);
    assert_eq!(text(stream.current()), "héllo");
    stream.close();

    let output = stream.into_output();
    assert!(matches!(output.outcome, Outcome::Completed { .. }));
    assert_eq!(output.usage.total_tokens, Some(12));
}

#[tokio::test]
async fn combined_provider_streams_delegate_both_protocols_and_cleanup() {
    for provider in [ProviderType::OpenAiCodex, ProviderType::Openrouter] {
        for mode in 0..3 {
            let body = match provider {
                ProviderType::OpenAiCodex => responses_stream(),
                ProviderType::Openrouter => chat_stream(true),
            };
            let (url, request) = server("200 OK", "text/event-stream", "", body).await;
            let client = provider_at_url(provider, url);

            let mut stream = client
                .generate(&history(provider), &input(), &RequestConfig::default())
                .await
                .unwrap();
            assert!(matches!(
                (&stream, provider),
                (ProviderStream::Responses(_), ProviderType::OpenAiCodex)
                    | (ProviderStream::Completions(_), ProviderType::Openrouter)
            ));

            match mode {
                0 => check_completed_stream(stream).await,
                1 => {
                    stream.close();
                    assert!(matches!(stream.current().outcome, Outcome::Cancelled));
                    assert!(stream.next_event().await.is_none());
                }
                _ => assert!(matches!(stream.into_output().outcome, Outcome::Cancelled)),
            }

            let body = request.await.unwrap().body;
            match provider {
                ProviderType::OpenAiCodex => assert_eq!(body["store"], false),
                ProviderType::Openrouter => assert_eq!(body["reasoning"]["effort"], "medium"),
            }
        }
    }
}

#[tokio::test]
async fn completions_http_round_trip() {
    let (url, request) = server(
        "200 OK",
        "text/event-stream; charset=utf-8",
        "",
        chat_stream(true),
    )
    .await;
    let client = OpenAiCompletions::new(
        url,
        "test-key",
        ProviderType::Openrouter,
        OpenAiCompletionsConfig {
            openrouter_reasoning_fields: true,
        },
    )
    .unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::Openrouter),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    assert!(matches!(stream.current().outcome, Outcome::InProgress));

    let mut finished = 0;
    while let Some(event) = stream.next_event().await {
        if matches!(event.unwrap(), TurnEvent::Finished) {
            finished += 1;
        }
    }

    assert_eq!(finished, 1);
    assert!(stream.next_event().await.is_none());

    let output = stream.into_output();
    assert_eq!(text(&output), "héllo");
    assert_eq!(output.response_id.as_deref(), Some("chat-1"));
    assert_eq!(output.usage.total_tokens, Some(12));
    assert!(matches!(output.outcome, Outcome::Completed { .. }));

    let request = request.await.unwrap();
    assert!(
        request
            .headers
            .starts_with("POST /custom-endpoint HTTP/1.1")
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key")
    );
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["messages"][0]["role"], "user");
}

#[tokio::test]
async fn responses_http_round_trip() {
    let (url, request) = server("200 OK", "text/event-stream", "", responses_stream()).await;
    let client = OpenAiResponses::new(url, "test-key", ProviderType::OpenAiCodex).unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::OpenAiCodex),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    let mut finished = 0;
    while let Some(event) = stream.next_event().await {
        if matches!(event.unwrap(), TurnEvent::Finished) {
            finished += 1;
        }
    }

    assert_eq!(finished, 1);

    let output = stream.into_output();
    assert_eq!(text(&output), "héllo");
    assert_eq!(output.response_id.as_deref(), Some("resp-1"));
    assert_eq!(output.usage.total_tokens, Some(12));
    assert!(matches!(output.outcome, Outcome::Completed { .. }));

    let request = request.await.unwrap();
    assert_eq!(request.body["store"], false);
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["include"][0], "reasoning.encrypted_content");
}

#[tokio::test]
async fn standard_completions_uses_supplied_identity_and_standard_reasoning_field() {
    let (url, request) = server("200 OK", "text/event-stream", "", chat_stream(true)).await;
    let client = OpenAiCompletions::new(
        url,
        "test-key",
        ProviderType::OpenAiCodex,
        OpenAiCompletionsConfig::default(),
    )
    .unwrap();

    assert_eq!(client.provider_type(), ProviderType::OpenAiCodex);

    let mut stream = client
        .generate(
            &history(ProviderType::OpenAiCodex),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    while let Some(event) = stream.next_event().await {
        event.unwrap();
    }

    assert_eq!(stream.current().config.provider, ProviderType::OpenAiCodex);
    assert_eq!(text(stream.current()), "héllo");

    let body = request.await.unwrap().body;
    assert_eq!(body["reasoning_effort"], "medium");
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn responses_retags_and_replays_native_state_for_supplied_identity() {
    let message = json!({
        "id":"msg-generic", "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":"source","annotations":[{
            "type":"url_citation","start_index":0,"end_index":6,"url":"https://example.com","title":"Source"
        }]}],
    });
    let terminal = json!({"type":"response.completed","response":{
        "id":"resp-generic","status":"completed","output":[
            {"id":"rs-generic","type":"reasoning","summary":[],"encrypted_content":"opaque-state"}, message,
        ],
    }});

    let (url, _) = server(
        "200 OK",
        "text/event-stream",
        "",
        format!("data: {terminal}\n\n"),
    )
    .await;
    let client = OpenAiResponses::new(url, "test-key", ProviderType::Openrouter).unwrap();
    assert_eq!(client.provider_type(), ProviderType::Openrouter);

    let mut stream = client
        .generate(
            &history(ProviderType::Openrouter),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    while let Some(event) = stream.next_event().await {
        event.unwrap();
    }

    let output = stream.into_output();
    let HistoryItemKind::Reasoning(reasoning) = &output.items[0].kind else {
        panic!("missing reasoning")
    };
    assert_eq!(
        reasoning.opaque_state.as_ref().unwrap().provider,
        ProviderType::Openrouter
    );

    let HistoryItemKind::Message(message) = &output.items[1].kind else {
        panic!("missing message")
    };
    let ContentBlock::Text(text) = &message.content[0] else {
        panic!("missing text")
    };
    assert_eq!(text.annotations[0].provider, ProviderType::Openrouter);

    let mut history = history(ProviderType::Openrouter);
    history.turns.push(ModelTurn {
        input: input(),
        output,
    });
    let (url, request) = server("200 OK", "text/event-stream", "", responses_stream()).await;
    let client = OpenAiResponses::new(url, "test-key", ProviderType::Openrouter).unwrap();

    let stream = client
        .generate(&history, &input(), &RequestConfig::default())
        .await
        .unwrap();
    drop(stream);

    let body = request.await.unwrap().body;
    assert_eq!(body["input"][1]["encrypted_content"], "opaque-state");
    assert_eq!(
        body["input"][2]["content"][0]["annotations"][0]["url"],
        "https://example.com"
    );
    assert_eq!(body["input"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn unexpected_eof_retains_partial_output_and_errors_once() {
    let (url, _) = server("200 OK", "text/event-stream", "", chat_stream(false)).await;
    let client = OpenAiCompletions::new(
        url,
        "test-key",
        ProviderType::Openrouter,
        OpenAiCompletionsConfig {
            openrouter_reasoning_fields: true,
        },
    )
    .unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::Openrouter),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    let mut errors = 0;
    while let Some(event) = stream.next_event().await {
        if event.is_err() {
            errors += 1;
        }
    }

    assert_eq!(errors, 1);
    assert!(stream.next_event().await.is_none());
    assert_eq!(text(stream.current()), "héllo");
    assert!(matches!(stream.current().outcome, Outcome::Failed(_)));
}

#[tokio::test]
async fn close_and_into_output_preserve_terminal_or_partial_state() {
    let (url, _) = server("200 OK", "text/event-stream", "", chat_stream(true)).await;
    let client = OpenAiCompletions::new(
        url,
        "test-key",
        ProviderType::Openrouter,
        OpenAiCompletionsConfig {
            openrouter_reasoning_fields: true,
        },
    )
    .unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::Openrouter),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    while text(stream.current()).is_empty() {
        stream.next_event().await.unwrap().unwrap();
    }

    let output = stream.into_output();
    assert_eq!(text(&output), "héllo");
    assert!(matches!(output.outcome, Outcome::Cancelled));

    let (url, _) = server("200 OK", "text/event-stream", "", responses_stream()).await;
    let client = OpenAiResponses::new(url, "test-key", ProviderType::OpenAiCodex).unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::OpenAiCodex),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    stream.close();
    stream.close();
    assert!(stream.next_event().await.is_none());
    assert!(matches!(stream.into_output().outcome, Outcome::Cancelled));
}

#[tokio::test]
async fn http_errors_retain_status_and_retry_delay() {
    let (url, _) = server(
        "429 Too Many Requests",
        "application/json",
        "Retry-After: 7\r\n",
        json!({"error":{"message":"rate limited"}}).to_string(),
    )
    .await;
    let client = OpenAiResponses::new(url, "test-key", ProviderType::OpenAiCodex).unwrap();
    let error = match client
        .generate(
            &history(ProviderType::OpenAiCodex),
            &input(),
            &RequestConfig::default(),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("HTTP failure accepted"),
    };

    assert!(matches!(
        error,
        Error::Http {
            status: 429,
            retry_after_seconds: Some(7),
            ..
        }
    ));
}

#[tokio::test]
async fn truncated_error_body_keeps_http_status() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 100\r\nRetry-After: 3\r\nConnection: close\r\n\r\n{}").await.unwrap();
    });

    let client = OpenAiResponses::new(
        format!("http://{address}/api"),
        "test-key",
        ProviderType::OpenAiCodex,
    )
    .unwrap();

    assert!(matches!(
        client
            .generate(
                &history(ProviderType::OpenAiCodex),
                &input(),
                &RequestConfig::default()
            )
            .await,
        Err(Error::Http {
            status: 503,
            retry_after_seconds: Some(3),
            ..
        })
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn dropping_a_stream_closes_the_http_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut byte = [0];
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => {}
            Ok(_) => panic!("unexpected client bytes after generation request"),
        }
    });

    let client = OpenAiResponses::new(
        format!("http://{address}/api"),
        "test-key",
        ProviderType::OpenAiCodex,
    )
    .unwrap();

    let stream = client
        .generate(
            &history(ProviderType::OpenAiCodex),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();
    drop(stream);

    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn validation_fails_before_http() {
    let client = OpenAiResponses::new(
        "http://127.0.0.1:9/no-server",
        "test-key",
        ProviderType::OpenAiCodex,
    )
    .unwrap();

    assert!(matches!(
        client
            .generate(
                &history(ProviderType::Openrouter),
                &input(),
                &RequestConfig::default()
            )
            .await,
        Err(Error::ProviderMismatch { .. })
    ));

    let options = RequestConfig {
        max_output_tokens: Some(0),
        ..RequestConfig::default()
    };
    assert!(matches!(
        client
            .generate(&history(ProviderType::OpenAiCodex), &input(), &options)
            .await,
        Err(Error::InvalidConfig { .. })
    ));
}

#[tokio::test]
async fn rejects_non_sse_success_response() {
    let (url, _) = server("200 OK", "application/json", "", "{}".into()).await;
    let client = OpenAiCompletions::new(
        url,
        "test-key",
        ProviderType::Openrouter,
        OpenAiCompletionsConfig {
            openrouter_reasoning_fields: true,
        },
    )
    .unwrap();

    assert!(matches!(
        client
            .generate(
                &history(ProviderType::Openrouter),
                &input(),
                &RequestConfig::default()
            )
            .await,
        Err(Error::Protocol { .. })
    ));
}

#[tokio::test]
async fn cancelling_next_event_does_not_lose_buffered_output() {
    // Headers and one partial SSE event arrive before a deliberate stall.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"chat-1\",").await.unwrap();
        wait.await.unwrap();
        socket.write_all(b"\"choices\":[{\"index\":0,\"delta\":{\"content\":\"resumed\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await.unwrap();
    });

    let client = OpenAiCompletions::new(
        format!("http://{address}/api"),
        "test-key",
        ProviderType::Openrouter,
        OpenAiCompletionsConfig {
            openrouter_reasoning_fields: true,
        },
    )
    .unwrap();

    let mut stream = client
        .generate(
            &history(ProviderType::Openrouter),
            &input(),
            &RequestConfig::default(),
        )
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next_event())
            .await
            .is_err()
    );

    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next_event().await {
            event.unwrap();
        }
    })
    .await
    .unwrap();
    task.await.unwrap();

    assert_eq!(text(stream.current()), "resumed");
    assert!(matches!(
        stream.current().outcome,
        Outcome::Completed { .. }
    ));
}
