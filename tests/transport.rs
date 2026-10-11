//! Transport tests for `OpenAiCompatClient` against a local `wiremock`
//! server: every retry, fallback, error-mapping and timeout path, plus the
//! exact request payload. No test talks to a real provider, and every client
//! uses `ProxyMode::None` (unless the proxy itself is under test), so the
//! developer's shell proxy settings never matter.

use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use munge_comments::comments::types::{Anchor, Position};
use munge_comments::llm::client::{
    CallKind, ChatRequest, LlmClient, LlmMeta, Message, MetaItem, OpenAiCompatClient,
    OpenAiCompatConfig, ProxyMode, RawResponse, RetryPolicy,
};
use munge_comments::{Error, LanguageId, LlmErrorReason, Mode};

const ENDPOINT: &str = "/v1/chat/completions";

/// Retries without waiting.
fn zero_delay_retry(max_retries: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

/// Config for a client pointed at `server`, with a test key, zero-delay
/// retries, a generous timeout and no proxy.
fn config(server: &MockServer) -> OpenAiCompatConfig {
    OpenAiCompatConfig {
        base_url: format!("{}/v1", server.uri()),
        api_key: Some("sk-test".to_string()),
        request_timeout: Duration::from_secs(10),
        retry: zero_delay_retry(3),
        proxy: ProxyMode::None,
    }
}

fn client(server: &MockServer) -> OpenAiCompatClient {
    OpenAiCompatClient::new(config(server)).unwrap()
}

/// A pinned request with routing metadata that must never be sent.
fn chat_request(temperature: Option<f64>) -> ChatRequest {
    ChatRequest {
        model: "gpt-4.1".to_string(),
        messages: vec![
            Message::system("Answer in JSON."),
            Message::user("Regenerate C1."),
        ],
        response_format: json!({
            "type": "json_schema",
            "json_schema": {
                "name": "RegenerationResponse",
                "strict": true,
                "schema": {"type": "object"}
            }
        }),
        temperature,
        max_completion_tokens: 16_384,
        meta: LlmMeta {
            call: CallKind::Regenerate,
            mode: Mode::Redact,
            path: "src/double.py".to_string(),
            language: LanguageId::Python,
            chunk_index: 0,
            ids: vec!["C1".to_string()],
            items: vec![MetaItem {
                id: "C1".to_string(),
                original: Some("# doubles the input".to_string()),
                regenerated: None,
                anchor: Anchor {
                    node_kind: "function_definition".to_string(),
                    name: Some("double".to_string()),
                    snippet: "def double(x):".to_string(),
                },
                position: Position::Leading,
            }],
        },
    }
}

/// A real-shaped success envelope, extra fields included.
fn success_body(content: &str) -> Value {
    json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "created": 1_760_000_000,
        "model": "gpt-4.1-2025-04-14",
        "system_fingerprint": "fp_abc123",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "refusal": null,
                "annotations": []
            },
            "logprobs": null,
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 0}
        },
        "service_tier": "default"
    })
}

fn ok(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(success_body(content))
}

/// An OpenAI-shaped error body.
fn openai_error(message: &str, param: Option<&str>) -> Value {
    json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": param,
            "code": null
        }
    })
}

/// The JSON bodies of every request the server received, in order.
async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|req| req.body_json::<Value>().unwrap())
        .collect()
}

async fn request_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn llm_reason(result: Result<RawResponse, Error>) -> (LlmErrorReason, String) {
    match result {
        Err(Error::Llm { reason, message }) => (reason, message),
        other => panic!("expected Error::Llm, got {other:?}"),
    }
}

fn has_key(req: &Request, key: &str) -> bool {
    req.body_json::<Value>()
        .map(|body| body.get(key).is_some())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Success and payload
// ---------------------------------------------------------------------

#[tokio::test]
async fn success_maps_first_choice_and_ignores_extra_envelope_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .respond_with(ok("{\"comments\": []}"))
        .expect(1)
        .mount(&server)
        .await;

    let response = client(&server)
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();
    assert_eq!(
        response,
        RawResponse {
            content: Some("{\"comments\": []}".to_string()),
            finish_reason: Some("stop".to_string()),
            refusal: None,
        }
    );

    let received = server.received_requests().await.unwrap();
    let headers = &received[0].headers;
    assert_eq!(headers["authorization"], "Bearer sk-test");
    assert_eq!(headers["content-type"], "application/json");
}

#[tokio::test]
async fn refusal_and_truncation_fields_are_surfaced() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "chatcmpl-9",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."},
            "finish_reason": "length"
        }],
        "usage": {"total_tokens": 1}
    });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let response = client(&server)
        .complete_json(&chat_request(None))
        .await
        .unwrap();
    assert_eq!(response.content, None);
    assert_eq!(response.refusal.as_deref(), Some("I can't help with that."));
    assert_eq!(response.finish_reason.as_deref(), Some("length"));
}

#[tokio::test]
async fn unparseable_or_choiceless_envelope_is_http_without_retry() {
    for body in [
        "not json at all",
        "{\"id\": \"x\", \"choices\": []}",
        "{\"id\": \"x\"}",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let (reason, message) =
            llm_reason(client(&server).complete_json(&chat_request(None)).await);
        assert_eq!(reason, LlmErrorReason::Http, "{body}");
        assert!(message.contains(body), "{message}");
        assert_eq!(request_count(&server).await, 1, "{body}");
    }
}

#[test]
fn payload_serializes_exactly_with_and_without_temperature() {
    let with = serde_json::to_string(&chat_request(Some(0.2))).unwrap();
    assert_eq!(
        with,
        concat!(
            r#"{"model":"gpt-4.1","#,
            r#""messages":[{"role":"system","content":"Answer in JSON."},"#,
            r#"{"role":"user","content":"Regenerate C1."}],"#,
            r#""response_format":{"json_schema":{"name":"RegenerationResponse","#,
            r#""schema":{"type":"object"},"strict":true},"type":"json_schema"},"#,
            r#""temperature":0.2,"max_completion_tokens":16384}"#,
        )
    );
    let without = serde_json::to_string(&chat_request(None)).unwrap();
    assert_eq!(
        without,
        concat!(
            r#"{"model":"gpt-4.1","#,
            r#""messages":[{"role":"system","content":"Answer in JSON."},"#,
            r#"{"role":"user","content":"Regenerate C1."}],"#,
            r#""response_format":{"json_schema":{"name":"RegenerationResponse","#,
            r#""schema":{"type":"object"},"strict":true},"type":"json_schema"},"#,
            r#""max_completion_tokens":16384}"#,
        )
    );
    assert!(!with.contains("meta") && !with.contains("doubles the input"));
}

#[tokio::test]
async fn transport_sends_exactly_the_serialized_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok("{}"))
        .mount(&server)
        .await;
    let client = client(&server);
    for temperature in [Some(0.2), None] {
        let req = chat_request(temperature);
        client.complete_json(&req).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let sent = &received.last().unwrap().body;
        assert_eq!(*sent, serde_json::to_vec(&req).unwrap());
    }
}

// ---------------------------------------------------------------------
// HTTP 400 fallbacks
// ---------------------------------------------------------------------

#[tokio::test]
async fn temperature_400_retries_without_it_and_later_requests_omit_it_up_front() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|req: &Request| has_key(req, "temperature"))
        .respond_with(ResponseTemplate::new(400).set_body_json(openai_error(
            "Unsupported value: 'temperature' does not support 0.2 with this model. \
             Only the default (1) value is supported.",
            Some("temperature"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| !has_key(req, "temperature"))
        .respond_with(ok("{}"))
        .mount(&server)
        .await;

    let client = client(&server);
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();

    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 3, "one fallback re-issue, then cached");
    assert_eq!(bodies[0]["temperature"], json!(0.2));
    assert!(bodies[1].get("temperature").is_none());
    assert!(bodies[2].get("temperature").is_none(), "omitted up front");
    // Nothing else changes.
    for body in &bodies {
        assert_eq!(body["max_completion_tokens"], json!(16_384));
        assert_eq!(body["response_format"]["type"], json!("json_schema"));
    }
}

#[tokio::test]
async fn max_completion_tokens_400_retries_with_max_tokens_then_cached() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|req: &Request| has_key(req, "max_completion_tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(openai_error(
            "Unsupported parameter: 'max_completion_tokens' is not supported with this model.",
            Some("max_completion_tokens"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| has_key(req, "max_tokens"))
        .respond_with(ok("{}"))
        .mount(&server)
        .await;

    let client = client(&server);
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();

    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 3, "one fallback re-issue, then cached");
    assert_eq!(bodies[0]["max_completion_tokens"], json!(16_384));
    assert!(bodies[0].get("max_tokens").is_none());
    for body in &bodies[1..] {
        assert_eq!(body["max_tokens"], json!(16_384));
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["temperature"], json!(0.2), "temperature untouched");
    }
}

#[tokio::test]
async fn schema_400_retries_with_json_object_then_cached() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|req: &Request| {
            req.body_json::<Value>()
                .is_ok_and(|body| body["response_format"]["type"] == "json_schema")
        })
        .respond_with(ResponseTemplate::new(400).set_body_json(openai_error(
            "Invalid schema for response_format 'RegenerationResponse': In context=(), \
             'additionalProperties' is required to be supplied and to be false.",
            Some("response_format"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| {
            req.body_json::<Value>()
                .is_ok_and(|body| body["response_format"] == json!({"type": "json_object"}))
        })
        .respond_with(ok("{}"))
        .mount(&server)
        .await;

    let client = client(&server);
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();
    client
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();

    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 3, "one fallback re-issue, then cached");
    assert_eq!(bodies[0]["response_format"]["type"], json!("json_schema"));
    for body in &bodies[1..] {
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }
}

#[tokio::test]
async fn fallbacks_compose_in_order_within_one_request() {
    // A server that rejects temperature, then max_completion_tokens, then
    // the strict schema: one request walks all three fallbacks once each.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|req: &Request| has_key(req, "temperature"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(openai_error("temperature unsupported", None)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| !has_key(req, "temperature") && has_key(req, "max_completion_tokens"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(openai_error("max_completion_tokens unsupported", None)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| {
            !has_key(req, "temperature")
                && has_key(req, "max_tokens")
                && req
                    .body_json::<Value>()
                    .is_ok_and(|body| body["response_format"]["type"] == "json_schema")
        })
        .respond_with(
            ResponseTemplate::new(400).set_body_json(openai_error("json_schema unsupported", None)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|req: &Request| {
            req.body_json::<Value>()
                .is_ok_and(|body| body["response_format"]["type"] == "json_object")
        })
        .respond_with(ok("{}"))
        .mount(&server)
        .await;

    client(&server)
        .complete_json(&chat_request(Some(0.2)))
        .await
        .unwrap();
    assert_eq!(request_count(&server).await, 4);
}

#[tokio::test]
async fn repeated_400_after_its_fallback_is_http() {
    // The server keeps complaining about temperature even once it is gone, so the fallback applies once and the second 400 surfaces as an error.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(openai_error("temperature is not supported", None)),
        )
        .mount(&server)
        .await;

    let (reason, message) = llm_reason(
        client(&server)
            .complete_json(&chat_request(Some(0.2)))
            .await,
    );
    assert_eq!(reason, LlmErrorReason::Http);
    assert!(
        message.contains("temperature is not supported"),
        "{message}"
    );
    assert_eq!(request_count(&server).await, 2, "the fallback applies once");
}

#[tokio::test]
async fn non_matching_400_is_http_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(openai_error(
            "Invalid 'messages[1].content': string too long.",
            Some("messages"),
        )))
        .mount(&server)
        .await;

    let (reason, message) = llm_reason(
        client(&server)
            .complete_json(&chat_request(Some(0.2)))
            .await,
    );
    assert_eq!(reason, LlmErrorReason::Http);
    assert!(message.contains("string too long"), "{message}");
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn non_openai_shaped_400_is_http_with_the_raw_body() {
    let server = MockServer::start().await;
    let raw = "<html><body>400 Bad Request: upstream gateway rejected payload</body></html>";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(raw))
        .mount(&server)
        .await;

    let (reason, message) = llm_reason(
        client(&server)
            .complete_json(&chat_request(Some(0.2)))
            .await,
    );
    assert_eq!(reason, LlmErrorReason::Http);
    assert!(message.contains(raw), "{message}");
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn error_bodies_are_truncated_to_two_kilobytes() {
    let server = MockServer::start().await;
    let raw = "é".repeat(3000); // 6000 bytes, two per char
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(raw.clone()))
        .mount(&server)
        .await;

    let (reason, message) = llm_reason(client(&server).complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Http);
    assert!(message.contains(&raw[..2048]), "first 2 KB quoted");
    assert!(!message.contains(&raw[..2050]), "nothing beyond 2 KB");
    assert!(message.contains("truncated"), "{message}");
}

// ---------------------------------------------------------------------
// Retries, auth, timeouts
// ---------------------------------------------------------------------

/// A client whose computed backoff is far longer than any test may wait,
/// so finishing promptly proves a server-requested delay was honored.
fn slow_backoff_client(server: &MockServer) -> OpenAiCompatClient {
    OpenAiCompatClient::new(OpenAiCompatConfig {
        retry: RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_secs(600),
            max_delay: Duration::from_secs(600),
        },
        ..config(server)
    })
    .unwrap()
}

async fn rate_limited_then_ok(header: (&str, &str)) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(header.0, header.1)
                .set_body_json(openai_error("Rate limit reached", None)),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok("{\"ok\": true}"))
        .with_priority(2)
        .mount(&server)
        .await;

    let client = slow_backoff_client(&server);
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client.complete_json(&chat_request(Some(0.2))),
    )
    .await
    .expect("the retry-after header must override the 600 s backoff")
    .unwrap();
    assert_eq!(response.content.as_deref(), Some("{\"ok\": true}"));
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn rate_limit_with_retry_after_ms_then_success() {
    rate_limited_then_ok(("retry-after-ms", "0")).await;
}

#[tokio::test]
async fn rate_limit_with_retry_after_seconds_then_success() {
    rate_limited_then_ok(("retry-after", "0")).await;
}

#[tokio::test]
async fn server_error_then_success_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok("{}"))
        .with_priority(2)
        .mount(&server)
        .await;

    client(&server)
        .complete_json(&chat_request(None))
        .await
        .unwrap();
    assert_eq!(request_count(&server).await, 3);
}

#[tokio::test]
async fn server_error_exhaustion_is_exhausted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        retry: zero_delay_retry(2),
        ..config(&server)
    })
    .unwrap();
    let (reason, message) = llm_reason(client.complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Exhausted);
    assert!(
        message.contains("503") && message.contains("overloaded"),
        "{message}"
    );
    assert_eq!(request_count(&server).await, 3, "first attempt + 2 retries");
}

#[tokio::test]
async fn auth_failures_are_immediate() {
    for status in [401, 403, 404] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(openai_error("Incorrect API key provided", None)),
            )
            .mount(&server)
            .await;

        let (reason, message) =
            llm_reason(client(&server).complete_json(&chat_request(None)).await);
        assert_eq!(reason, LlmErrorReason::Auth, "HTTP {status}");
        assert!(message.contains(&status.to_string()), "{message}");
        assert_eq!(request_count(&server).await, 1, "HTTP {status}: no retry");
    }
}

#[tokio::test]
async fn other_client_errors_are_http_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(422).set_body_string("unprocessable"))
        .mount(&server)
        .await;

    let (reason, _) = llm_reason(client(&server).complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Http);
    assert_eq!(request_count(&server).await, 1);
}

/// A client with a 50 ms timeout against a server that answers in 2 s.
async fn delayed_server_and_client(max_retries: u32) -> (MockServer, OpenAiCompatClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok("{}").set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        request_timeout: Duration::from_millis(50),
        retry: zero_delay_retry(max_retries),
        ..config(&server)
    })
    .unwrap();
    (server, client)
}

#[tokio::test]
async fn timeout_without_retries_is_timeout() {
    let (server, client) = delayed_server_and_client(0).await;
    let (reason, _) = llm_reason(client.complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Timeout);
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn exhaustion_ending_in_a_timeout_is_timeout_not_exhausted() {
    let (server, client) = delayed_server_and_client(2).await;
    let (reason, _) = llm_reason(client.complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Timeout);
    assert_eq!(request_count(&server).await, 3, "timeouts are retried");
}

#[tokio::test]
async fn connection_errors_are_retried_then_exhausted() {
    // Bind a port, then free it: connecting to it is refused.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: None,
        request_timeout: Duration::from_secs(5),
        retry: zero_delay_retry(1),
        proxy: ProxyMode::None,
    })
    .unwrap();
    let (reason, message) = llm_reason(client.complete_json(&chat_request(None)).await);
    assert_eq!(reason, LlmErrorReason::Exhausted);
    assert!(message.contains("2 attempt"), "{message}");
}

// ---------------------------------------------------------------------
// Keys and proxies
// ---------------------------------------------------------------------

#[tokio::test]
async fn base_url_without_key_sends_bearer_unused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .respond_with(ok("{}"))
        .mount(&server)
        .await;
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        // A trailing slash on the base URL is tolerated: it is normalized away so the request path is not doubled.
        base_url: format!("{}/v1/", server.uri()),
        api_key: None,
        ..config(&server)
    })
    .unwrap();

    client.complete_json(&chat_request(None)).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].headers["authorization"], "Bearer unused");
}

#[tokio::test]
async fn proxy_url_routes_requests_through_the_proxy() {
    // The mock server plays the proxy: the base URL names an unresolvable
    // host, so the request can only succeed by going through the proxy.
    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .respond_with(ok("{\"via\": \"proxy\"}"))
        .mount(&proxy)
        .await;
    let client = OpenAiCompatClient::new(OpenAiCompatConfig {
        base_url: "http://llm.upstream.invalid/v1".to_string(),
        proxy: ProxyMode::Url(proxy.uri()),
        ..config(&proxy)
    })
    .unwrap();

    let response = client.complete_json(&chat_request(None)).await.unwrap();
    assert_eq!(response.content.as_deref(), Some("{\"via\": \"proxy\"}"));
    let received = proxy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.host_str(), Some("llm.upstream.invalid"));
}

#[test]
fn system_proxy_mode_constructs() {
    // Construction only: no request is made.
    OpenAiCompatClient::new(OpenAiCompatConfig {
        api_key: Some("sk-test".to_string()),
        proxy: ProxyMode::System,
        ..OpenAiCompatConfig::default()
    })
    .unwrap();
}

#[test]
fn invalid_configuration_is_a_usage_error() {
    let bad = [
        OpenAiCompatConfig {
            base_url: "ftp://example.com/v1".to_string(),
            ..OpenAiCompatConfig::default()
        },
        OpenAiCompatConfig {
            proxy: ProxyMode::Url("::not a proxy::".to_string()),
            ..OpenAiCompatConfig::default()
        },
        OpenAiCompatConfig {
            api_key: Some("sk-with\nnewline".to_string()),
            ..OpenAiCompatConfig::default()
        },
    ];
    for config in bad {
        let shown = format!("{config:?}");
        let err = OpenAiCompatClient::new(config).unwrap_err();
        assert!(matches!(err, Error::Usage(_)), "{shown}: {err:?}");
    }
}

#[test]
fn debug_output_redacts_the_key() {
    let config = OpenAiCompatConfig {
        api_key: Some("sk-secret".to_string()),
        ..OpenAiCompatConfig::default()
    };
    assert!(!format!("{config:?}").contains("sk-secret"));
    let client = OpenAiCompatClient::new(config).unwrap();
    assert!(!format!("{client:?}").contains("sk-secret"));
}

#[test]
fn defaults_match_the_design() {
    let config = OpenAiCompatConfig::default();
    assert_eq!(config.base_url, "https://api.openai.com/v1");
    assert_eq!(config.api_key, None);
    assert_eq!(config.request_timeout, Duration::from_secs(120));
    assert_eq!(config.proxy, ProxyMode::None);
    assert_eq!(
        config.retry,
        RetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    );
}
