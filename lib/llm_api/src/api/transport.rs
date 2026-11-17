//! Shared authenticated HTTP and cancellation-safe SSE transport.

use std::collections::VecDeque;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::Value;

use super::{ModelResponse, Outcome, TurnEvent, Usage};
use crate::Error;

/// Maximum buffered SSE event size, including generated media.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct ApiClient {
    client: reqwest::Client,
    url: reqwest::Url,
}

impl ApiClient {
    /// The URL is the complete endpoint, not a base URL; no path is appended.
    pub(super) fn new(url: impl Into<String>, key: impl Into<String>) -> Result<Self, Error> {
        let url = parse_endpoint_url(&url.into())?;
        let headers = build_authenticated_headers(&key.into())?;
        let client = reqwest::Client::builder()
            .default_headers(headers)
            // Do not forward secrets or silently change request semantics on redirects.
            .redirect(reqwest::redirect::Policy::none())
            // A retry could create a second billable model request.
            .retry(reqwest::retry::never())
            .build()?;

        Ok(Self { client, url })
    }

    pub(super) async fn post(&self, body: &Value) -> Result<reqwest::Response, Error> {
        let response = self.client.post(self.url.clone()).json(body).send().await?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }

        let is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"));
        if !is_sse {
            return Err(protocol("expected a text/event-stream HTTP response"));
        }

        Ok(response)
    }
}

pub(super) fn parse_endpoint_url(value: &str) -> Result<reqwest::Url, Error> {
    let url = reqwest::Url::parse(value).map_err(|_| Error::InvalidConfig {
        field: "api_url".into(),
        message: "expected an absolute HTTP(S) endpoint URL".into(),
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::InvalidConfig {
            field: "api_url".into(),
            message: "expected an HTTP(S) URL without userinfo or fragment".into(),
        });
    }

    Ok(url)
}

pub(super) fn build_authenticated_headers(key: &str) -> Result<HeaderMap, Error> {
    if key.is_empty() || key.trim() != key {
        return Err(Error::InvalidConfig {
            field: "api_key".into(),
            message: "key must be nonempty without surrounding whitespace".into(),
        });
    }

    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| Error::InvalidConfig {
            field: "api_key".into(),
            message: "key contains invalid header characters".into(),
        })?;
    authorization.set_sensitive(true);

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

    Ok(headers)
}

pub(super) async fn http_error(mut response: reqwest::Response) -> Error {
    let status = response.status().as_u16();
    let retry_after_seconds = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| parse_retry_after(value, std::time::SystemTime::now()));
    let (bytes, read_error) = read_error_body(&mut response).await;

    let mut message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    if let Some(error) = read_error {
        message.push_str(&format!(" [could not read complete error body: {error}]"));
    }

    Error::Http {
        status,
        message,
        retry_after_seconds,
    }
}

async fn read_error_body(response: &mut reqwest::Response) -> (Vec<u8>, Option<String>) {
    let mut bytes = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return (bytes, None),
            Err(error) => return (bytes, Some(error.without_url().to_string())),
        };

        let remaining = MAX_ERROR_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() == MAX_ERROR_BYTES {
            return (bytes, None);
        }
    }
}

#[derive(Debug)]
pub(super) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub(super) trait Decoder: Send {
    /// Return true only after observing a valid provider terminal marker.
    /// Transport appends Finished and closes the response after draining events.
    fn decode(
        &mut self,
        event: SseEvent,
        output: &mut ModelResponse,
        events: &mut VecDeque<TurnEvent>,
    ) -> Result<bool, Error>;
}

pub(super) struct StreamingResponse<P> {
    response: Option<reqwest::Response>,
    parser: P,
    sse: SseDecoder,
    output: ModelResponse,
    events: VecDeque<TurnEvent>,
    terminal: bool,
}

impl<P: Decoder> StreamingResponse<P> {
    pub(super) fn new(response: reqwest::Response, output: ModelResponse, parser: P) -> Self {
        Self {
            response: Some(response),
            parser,
            sse: SseDecoder::default(),
            output,
            events: VecDeque::new(),
            terminal: false,
        }
    }

    pub(super) fn current(&self) -> &ModelResponse {
        &self.output
    }

    pub(super) async fn next_event(&mut self) -> Option<Result<TurnEvent, Error>> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Some(Ok(event));
            }
            if self.terminal {
                return None;
            }

            let result = match self.sse.next_event() {
                Err(error) => Err(error),
                Ok(Some(event)) => self.decode_event(event),
                Ok(None) => self.read_chunk().await,
            };
            if let Err(error) = result {
                return self.fail(error);
            }
        }
    }

    fn decode_event(&mut self, event: SseEvent) -> Result<(), Error> {
        if !self
            .parser
            .decode(event, &mut self.output, &mut self.events)?
        {
            return Ok(());
        }
        if matches!(self.output.outcome, Outcome::InProgress) {
            return Err(protocol("terminal event has no terminal outcome"));
        }

        self.terminal = true;
        self.response = None;
        self.events.push_back(TurnEvent::Finished);

        Ok(())
    }

    async fn read_chunk(&mut self) -> Result<(), Error> {
        // The response body, decoder buffers, and queued events live
        // on self. Cancelling this await does not discard local state.
        let response = self
            .response
            .as_mut()
            .ok_or_else(|| protocol("missing HTTP response before stream termination"))?;
        let bytes = response
            .chunk()
            .await?
            .ok_or_else(|| protocol("unexpected EOF before provider terminal event"))?;

        self.sse.feed(&bytes)
    }

    fn fail(&mut self, error: Error) -> Option<Result<TurnEvent, Error>> {
        self.output.outcome = Outcome::Failed(error.clone());
        self.response = None;
        self.events.clear();
        self.sse = SseDecoder::default();
        self.terminal = true;
        Some(Err(error))
    }

    pub(super) fn close(&mut self) {
        self.response = None;
        self.events.clear();
        self.sse = SseDecoder::default();
        self.terminal = true;

        if matches!(self.output.outcome, Outcome::InProgress) {
            self.output.outcome = Outcome::Cancelled;
        }
    }

    pub(super) fn into_output(mut self) -> ModelResponse {
        self.close();
        self.output
    }
}

// No background task is spawned. Dropping the handle drops reqwest's response
// body, immediately abandoning local reads. Explicit close permits inspecting
// the cancelled snapshot afterwards; Drop does not need a custom implementation.

#[derive(Default)]
struct SseDecoder {
    bytes: VecDeque<u8>,
    line: Vec<u8>,
    data: Vec<String>,
    event: Option<String>,
    event_bytes: usize,
    skip_lf: bool,
    started: bool,
}

impl SseDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_EVENT_BYTES {
            return Err(protocol("SSE buffer exceeds 16 MiB limit"));
        }

        self.bytes.extend(bytes);
        Ok(())
    }

    fn next_event(&mut self) -> Result<Option<SseEvent>, Error> {
        while let Some(byte) = self.bytes.pop_front() {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\r' | b'\n' => {
                    self.skip_lf = byte == b'\r';
                    if let Some(event) = self.finish_line()? {
                        return Ok(Some(event));
                    }
                }
                _ => {
                    self.line.push(byte);
                    if self.line.len().saturating_add(self.event_bytes) > MAX_EVENT_BYTES {
                        return Err(protocol("SSE event exceeds 16 MiB limit"));
                    }
                }
            }
        }
        Ok(None)
    }

    fn finish_line(&mut self) -> Result<Option<SseEvent>, Error> {
        let bytes = std::mem::take(&mut self.line);
        let line =
            std::str::from_utf8(&bytes).map_err(|_| protocol("invalid UTF-8 in SSE event"))?;
        let line = if !self.started {
            self.started = true;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };

        if line.is_empty() {
            return Ok(self.finish_event());
        }
        if line.starts_with(':') {
            return Ok(None);
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        self.event_bytes = self.event_bytes.saturating_add(line.len());
        if self.event_bytes > MAX_EVENT_BYTES {
            return Err(protocol("SSE event exceeds 16 MiB limit"));
        }

        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            // Reconnect IDs and retry directives are not used: no automatic retry.
            _ => {}
        }
        Ok(None)
    }

    fn finish_event(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        self.event_bytes = 0;
        if self.data.is_empty() {
            return None;
        }

        let data = std::mem::take(&mut self.data).join("\n");
        Some(SseEvent { event, data })
    }
}

fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<u64> {
    if let Ok(seconds) = value.parse() {
        return Some(seconds);
    }

    let date = httpdate::parse_http_date(value).ok()?;
    let delay = date.duration_since(now).unwrap_or_default();
    Some(
        delay
            .as_secs()
            .saturating_add(u64::from(delay.subsec_nanos() != 0)),
    )
}

pub(super) fn protocol(message: impl Into<String>) -> Error {
    Error::Protocol {
        message: message.into(),
    }
}

pub(super) fn provider_error(value: &Value) -> Error {
    let error = value
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(value);

    Error::Provider {
        code: error.get("code").and_then(Value::as_str).map(str::to_owned),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider stream error")
            .to_owned(),
        metadata: error
            .as_object()
            .map(|fields| {
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

pub(super) fn parse_usage(value: &Value, chat: bool) -> Usage {
    let input = if chat {
        "prompt_tokens"
    } else {
        "input_tokens"
    };
    let output = if chat {
        "completion_tokens"
    } else {
        "output_tokens"
    };

    Usage {
        input_tokens: value.get(input).and_then(Value::as_u64),
        output_tokens: value.get(output).and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        cached_input_tokens: value
            .get(format!("{input}_details"))
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_tokens: value
            .get(format!("{input}_details"))
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(Value::as_u64),
        reasoning_output_tokens: value
            .get(format!("{output}_details"))
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cost: value.get("cost").and_then(Value::as_f64),
        provider_details: value
            .as_object()
            .map(|fields| {
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fragmented_utf8_crlf_comments_and_multiple_data_lines() {
        let mut decoder = SseDecoder::default();
        let bytes =
            "\u{feff}: heartbeat\r\nevent: test\r\ndata: héllo\r\ndata: world\r\n\r\n".as_bytes();
        let mut parsed = Vec::new();
        for byte in bytes {
            decoder.feed(&[*byte]).unwrap();
            while let Some(event) = decoder.next_event().unwrap() {
                parsed.push(event);
            }
        }
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].event.as_deref(), Some("test"));
        assert_eq!(parsed[0].data, "héllo\nworld");
    }

    #[test]
    fn parses_cr_only_and_empty_data_events() {
        let mut decoder = SseDecoder::default();
        decoder
            .feed(b"event: ignored\r\rdata:\r\rdata: second\n\n")
            .unwrap();
        let event = decoder.next_event().unwrap().unwrap();
        assert_eq!(event.event, None);
        assert_eq!(event.data, "");
        assert_eq!(decoder.next_event().unwrap().unwrap().data, "second");
        assert!(decoder.next_event().unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut decoder = SseDecoder::default();
        decoder.feed(b"data: \xff\n\n").unwrap();
        assert!(decoder.next_event().is_err());
    }

    #[test]
    fn rejects_invalid_credentials_without_exposing_them() {
        let error = match ApiClient::new("https://example.com/v1/responses", "secret\nkey") {
            Err(error) => error,
            Ok(_) => panic!("invalid key accepted"),
        };
        assert!(!error.to_string().contains("secret"));
        assert!(ApiClient::new("file:///tmp/api", "key").is_err());
    }

    #[test]
    fn retry_after_supports_seconds_and_http_dates() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        assert_eq!(parse_retry_after("7", now), Some(7));
        let date = httpdate::fmt_http_date(now + std::time::Duration::from_secs(10));
        assert_eq!(parse_retry_after(&date, now), Some(10));
        assert_eq!(
            parse_retry_after(&date, now + std::time::Duration::from_secs(20)),
            Some(0)
        );
        assert_eq!(parse_retry_after("invalid", now), None);
    }

    #[tokio::test]
    async fn cancelled_poll_keeps_partial_sse_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let size = socket.read(&mut chunk).await.unwrap();
                assert_ne!(size, 0);
                bytes.extend_from_slice(&chunk[..size]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: par").await.unwrap();
            wait.await.unwrap();
            socket.write_all(b"tial\n\n").await.unwrap();
        });
        struct TerminalDecoder;
        impl Decoder for TerminalDecoder {
            fn decode(
                &mut self,
                event: SseEvent,
                output: &mut ModelResponse,
                _: &mut VecDeque<TurnEvent>,
            ) -> Result<bool, Error> {
                assert_eq!(event.data, "partial");
                output.outcome = Outcome::Completed {
                    reason: super::super::StopReason::EndTurn,
                };
                Ok(true)
            }
        }
        let response = reqwest::get(format!("http://{address}")).await.unwrap();
        let output = ModelResponse::new(crate::providers::ModelConfig {
            model: crate::providers::Model::new(
                crate::providers::ProviderType::OpenAiCodex,
                "test-model",
            )
            .unwrap(),
            effort: crate::providers::Effort::Medium,
        });
        let mut stream = StreamingResponse::new(response, output, TerminalDecoder);
        // Repeatedly cancel a Pending poll until the actual fragment has been
        // ingested. Inspecting the buffer proves cancellation retained bytes.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let mut future = std::pin::pin!(stream.next_event());
                    std::future::poll_fn(|cx| {
                        assert!(future.as_mut().poll(cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                }
                if stream.sse.line == b"data: par" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(stream.sse.line, b"data: par");
        release.send(()).unwrap();
        assert!(matches!(
            stream.next_event().await,
            Some(Ok(TurnEvent::Finished))
        ));
        assert!(stream.next_event().await.is_none());
        server.await.unwrap();
    }

    #[test]
    fn usage_counts_are_subsets_and_absent_counts_stay_absent() {
        let usage = parse_usage(
            &serde_json::json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}),
            true,
        );
        assert_eq!(usage.total_tokens, Some(15));
        assert_eq!(usage.cached_input_tokens, Some(3));
        assert_eq!(usage.reasoning_output_tokens, Some(2));
        assert_eq!(super::parse_usage(&Value::Null, false).input_tokens, None);
    }
}
