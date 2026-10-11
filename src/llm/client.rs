//! The LLM client contract and the built-in OpenAI-compatible transport.
//!
//! [`LlmClient`] is the single seam between the engine and a model: one
//! object-safe method, [`LlmClient::complete_json`], so `Arc<dyn LlmClient>`
//! admits the built-in [`OpenAiCompatClient`], test fakes, and
//! consumer-provided clients alike.
//!
//! [`OpenAiCompatClient`] speaks exactly one dialect — non-streaming
//! OpenAI-compatible Chat Completions — and owns its behaviors outright:
//!
//! - **Retries.** Only HTTP 429, HTTP 5xx, connection errors and timeouts
//!   are retried, up to [`RetryPolicy::max_retries`] times after the first
//!   attempt, with exponential backoff and jitter. A `retry-after-ms` or
//!   `retry-after` (seconds) response header overrides the computed delay.
//!   Exhaustion yields [`LlmErrorReason::Timeout`] when the last failure was
//!   a timeout, else [`LlmErrorReason::Exhausted`].
//! - **Auth.** HTTP 401/403/404 fail immediately with
//!   [`LlmErrorReason::Auth`].
//! - **Message-keyed HTTP 400 fallbacks**, cached per client instance:
//!   omit `temperature`; send `max_tokens` instead of
//!   `max_completion_tokens`; downgrade `response_format` to
//!   `{"type": "json_object"}`. Each applies at most once per request; any
//!   other 400 is [`LlmErrorReason::Http`] carrying the raw body (≤ 2 KB).
//! - **No ambient reads.** The proxy is explicit ([`ProxyMode`]); only
//!   [`ProxyMode::System`] lets reqwest consult the environment and OS proxy
//!   settings.
//!
//! Request bodies are serialized with `serde_json` and responses parsed with
//! it (reqwest's `json` feature is deliberately not enabled). The response
//! envelope structs are lenient — unknown fields such as `id`, `usage` and
//! `system_fingerprint` are ignored.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::comments::types::{Anchor, Position};
use crate::config::{DEFAULT_BASE_URL, Mode};
use crate::errors::{Error, LlmErrorReason};
use crate::lang::ids::LanguageId;

/// A chat-completions backend.
///
/// Object-safe, so `Arc<dyn LlmClient>` admits the built-in transport, the
/// test fake, and consumer-provided clients alike. Implementations return
/// the first choice of a non-streaming completion; interpreting it (refusal,
/// truncation, JSON parsing and validation) is the caller's job.
pub trait LlmClient: Send + Sync {
    /// Sends one chat-completions request and returns the first choice.
    fn complete_json<'a>(
        &'a self,
        req: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<RawResponse, Error>>;
}

/// The author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Instructions to the model.
    System,
    /// The caller's input.
    User,
    /// A previous model reply (used by the repair retry).
    Assistant,
}

/// One chat message, serialized as `{"role": …, "content": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who authored the message.
    pub role: Role,
    /// The message text.
    pub content: String,
}

impl Message {
    /// A `system` message.
    pub fn system(content: impl Into<String>) -> Message {
        Message {
            role: Role::System,
            content: content.into(),
        }
    }

    /// A `user` message.
    pub fn user(content: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
        }
    }

    /// An `assistant` message.
    pub fn assistant(content: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// A chat-completions request.
///
/// Serializes to exactly the JSON body sent to the endpoint (field order
/// included) while no 400 fallback is in effect; [`meta`](Self::meta) is
/// never serialized.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatRequest {
    /// The model name.
    pub model: String,
    /// The conversation.
    pub messages: Vec<Message>,
    /// The `response_format` object (a strict `json_schema` object built by
    /// the typed-call layer).
    pub response_format: serde_json::Value,
    /// Sampling temperature; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Output token cap. Sent as `max_tokens` instead once the
    /// `max_completion_tokens` fallback is in effect.
    pub max_completion_tokens: u32,
    /// Test-routing data for fakes. Never sent over the wire.
    #[serde(skip)]
    pub meta: LlmMeta,
}

/// The sampling temperature of every engine request.
pub const DEFAULT_TEMPERATURE: f64 = 0.2;

/// The output token cap of every engine request.
pub const DEFAULT_MAX_COMPLETION_TOKENS: u32 = 16_384;

impl ChatRequest {
    /// An engine request for `model` with the default sampling parameters
    /// ([`DEFAULT_TEMPERATURE`], [`DEFAULT_MAX_COMPLETION_TOKENS`]) and a
    /// null `response_format`, which the typed-call layer
    /// ([`complete_typed`](crate::llm::call::complete_typed)) fills in.
    pub fn new(model: impl Into<String>, messages: Vec<Message>, meta: LlmMeta) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages,
            response_format: serde_json::Value::Null,
            temperature: Some(DEFAULT_TEMPERATURE),
            max_completion_tokens: DEFAULT_MAX_COMPLETION_TOKENS,
            meta,
        }
    }
}

/// The first choice of a completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawResponse {
    /// `choices[0].message.content`.
    pub content: Option<String>,
    /// `choices[0].finish_reason`; `"length"` means truncated.
    pub finish_reason: Option<String>,
    /// `choices[0].message.refusal`.
    pub refusal: Option<String>,
}

/// Which engine call a request belongs to. Deserializes from `"regenerate"`
/// / `"judge"` (as in `FakeLlm` scenario files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallKind {
    /// Call #1: blind regeneration.
    Regenerate,
    /// Call #2: the judge.
    Judge,
}

/// Routing data that lets test doubles answer a request without parsing
/// its prompt (the blind regeneration prompt deliberately lacks the
/// original comment text). Never sent to an endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmMeta {
    /// The engine call this request belongs to.
    pub call: CallKind,
    /// The file's effective mode (selects the response shape).
    pub mode: Mode,
    /// Root-relative POSIX path of the file.
    pub path: String,
    /// The file's language.
    pub language: LanguageId,
    /// Index of the chunk within the file (0 for an unchunked file); for
    /// a judge call, the index of its batch within the file (the halves of
    /// a truncated batch keep their batch's index).
    pub chunk_index: usize,
    /// Redact mode: the requested placeholder ids; judge: the packet ids.
    pub ids: Vec<String>,
    /// Per-comment routing data.
    pub items: Vec<MetaItem>,
}

/// Per-comment routing data inside [`LlmMeta`].
#[derive(Debug, Clone, PartialEq)]
pub struct MetaItem {
    /// The comment id (e.g. `C12`).
    pub id: String,
    /// Raw original comment text; `None` for NEW-origin comments.
    pub original: Option<String>,
    /// The regenerated text (judge calls only).
    pub regenerated: Option<String>,
    /// The code the comment annotates.
    pub anchor: Anchor,
    /// Where the comment sits relative to its anchor.
    pub position: Position,
}

/// Retry policy for retryable failures (HTTP 429, HTTP 5xx, connection
/// errors, timeouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// Backoff before retry `n` (0-based) is `base_delay · 2^n` …
    pub base_delay: Duration,
    /// … capped at `max_delay`.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 4 retries, 500 ms base delay, 8 s cap.
    fn default() -> RetryPolicy {
        RetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// How the HTTP client chooses a proxy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxyMode {
    /// No proxy; the environment is never consulted (library default).
    #[default]
    None,
    /// reqwest's own behavior: `HTTP(S)_PROXY`/`NO_PROXY` and the OS proxy
    /// settings. The CLI uses this.
    System,
    /// Exactly this proxy URL, for every request.
    Url(String),
}

/// Configuration for [`OpenAiCompatClient`].
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiCompatConfig {
    /// Endpoint base URL; requests go to `{base_url}/chat/completions`.
    pub base_url: String,
    /// Bearer key; `None` sends `Bearer unused` (local servers).
    pub api_key: Option<String>,
    /// Total per-request timeout (connect through end of body).
    pub request_timeout: Duration,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// Proxy selection.
    pub proxy: ProxyMode,
}

impl Default for OpenAiCompatConfig {
    /// The OpenAI endpoint, no key, 120 s timeout, the default
    /// [`RetryPolicy`], and no proxy.
    fn default() -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            retry: RetryPolicy::default(),
            proxy: ProxyMode::None,
        }
    }
}

/// Debug output redacts the API key.
impl fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("request_timeout", &self.request_timeout)
            .field("retry", &self.retry)
            .field("proxy", &self.proxy)
            .finish()
    }
}

/// Maximum bytes of a raw error body quoted in an error message.
const MAX_BODY_IN_MESSAGE: usize = 2048;

/// The built-in OpenAI-compatible Chat Completions transport.
///
/// Holds one `reqwest::Client` (rustls, per-request total timeout, explicit
/// proxy) and the run-cached 400 fallbacks. `Send + Sync`; share it as
/// `Arc<dyn LlmClient>`.
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    endpoint: String,
    authorization: HeaderValue,
    retry: RetryPolicy,
    /// Fallback 1 is cached: omit `temperature` on every request.
    omit_temperature: AtomicBool,
    /// Fallback 2 is cached: send `max_tokens` instead of
    /// `max_completion_tokens`.
    legacy_max_tokens: AtomicBool,
    /// Fallback 3 is cached: send `response_format: {"type": "json_object"}`.
    json_object: AtomicBool,
}

impl fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatClient")
            .field("endpoint", &self.endpoint)
            .field("retry", &self.retry)
            .field("downgrades", &self.cached())
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatClient {
    /// Builds the client.
    ///
    /// Errors: [`Error::Usage`] for a base URL that is not an absolute
    /// `http`/`https` URL, an invalid [`ProxyMode::Url`], or an API key that
    /// cannot travel in an HTTP header; [`Error::Llm`] (`Http`) if the HTTP
    /// client itself cannot be constructed (e.g. TLS initialization).
    pub fn new(config: OpenAiCompatConfig) -> Result<OpenAiCompatClient, Error> {
        let endpoint = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        match reqwest::Url::parse(&endpoint) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => {}
            _ => {
                return Err(Error::Usage(format!(
                    "invalid base URL '{}': expected an absolute http(s) URL",
                    config.base_url
                )));
            }
        }

        let key = config.api_key.as_deref().unwrap_or("unused");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
            Error::Usage("the API key contains characters not allowed in an HTTP header".into())
        })?;
        authorization.set_sensitive(true);

        let builder = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .tls_backend_rustls();
        let builder = match &config.proxy {
            ProxyMode::None => builder.no_proxy(),
            ProxyMode::System => builder,
            ProxyMode::Url(url) => {
                let proxy = reqwest::Proxy::all(url.as_str())
                    .map_err(|err| Error::Usage(format!("invalid proxy URL '{url}': {err}")))?;
                builder.proxy(proxy)
            }
        };
        let http = builder.build().map_err(|err| Error::Llm {
            reason: LlmErrorReason::Http,
            message: format!("cannot construct the HTTP client: {}", error_chain(&err)),
        })?;

        Ok(OpenAiCompatClient {
            http,
            endpoint,
            authorization,
            retry: config.retry,
            omit_temperature: AtomicBool::new(false),
            legacy_max_tokens: AtomicBool::new(false),
            json_object: AtomicBool::new(false),
        })
    }

    /// The fallbacks cached on this client so far.
    fn cached(&self) -> Downgrades {
        Downgrades {
            omit_temperature: self.omit_temperature.load(Ordering::Relaxed),
            legacy_max_tokens: self.legacy_max_tokens.load(Ordering::Relaxed),
            json_object: self.json_object.load(Ordering::Relaxed),
        }
    }

    /// Caches a fallback for every later request on this client.
    fn cache(&self, fallback: Fallback) {
        let flag = match fallback {
            Fallback::OmitTemperature => &self.omit_temperature,
            Fallback::LegacyMaxTokens => &self.legacy_max_tokens,
            Fallback::JsonObject => &self.json_object,
        };
        flag.store(true, Ordering::Relaxed);
    }

    /// One request, with retries and 400 fallbacks.
    async fn complete(&self, req: &ChatRequest) -> Result<RawResponse, Error> {
        // Fallbacks applied while serving this request. Cached flags may also
        // be set concurrently by other requests; both are honored.
        let mut applied = Downgrades::default();
        let mut retries_used: u32 = 0;
        loop {
            let downgrades = self.cached().union(applied);
            let body = encode_body(req, downgrades)?;
            match self.send_once(body).await {
                Attempt::Done(response) => return Ok(response),
                Attempt::Fatal(err) => return Err(err),
                Attempt::BadRequest(body) => match choose_fallback(&body, req, downgrades) {
                    Some(fallback) => {
                        applied.set(fallback);
                        self.cache(fallback);
                    }
                    None => {
                        return Err(Error::Llm {
                            reason: LlmErrorReason::Http,
                            message: format!("HTTP 400: {}", truncate_body(&body)),
                        });
                    }
                },
                Attempt::Retryable {
                    timeout,
                    retry_after,
                    detail,
                } => {
                    if retries_used >= self.retry.max_retries {
                        let attempts = retries_used + 1;
                        return Err(Error::Llm {
                            reason: if timeout {
                                LlmErrorReason::Timeout
                            } else {
                                LlmErrorReason::Exhausted
                            },
                            message: format!(
                                "gave up after {attempts} attempt(s); last failure: {detail}"
                            ),
                        });
                    }
                    let delay = retry_after.unwrap_or_else(|| self.backoff(retries_used));
                    retries_used += 1;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    }

    /// Sends one HTTP request and classifies the outcome.
    async fn send_once(&self, body: Vec<u8>) -> Attempt {
        let sent = self
            .http
            .post(&self.endpoint)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(err) => return classify_transport_error(&err),
        };
        let status = response.status();
        let retry_after = retry_after(response.headers());
        let text = response.text().await;

        if status.is_success() {
            return match text {
                Ok(text) => parse_envelope(&text),
                Err(err) => classify_transport_error(&err),
            };
        }
        let body = text.unwrap_or_else(|err| format!("<unreadable body: {}>", error_chain(&err)));
        match status {
            StatusCode::BAD_REQUEST => Attempt::BadRequest(body),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                Attempt::Fatal(Error::Llm {
                    reason: LlmErrorReason::Auth,
                    message: format!("HTTP {status}: {}", truncate_body(&body)),
                })
            }
            _ if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                Attempt::Retryable {
                    timeout: false,
                    retry_after,
                    detail: format!("HTTP {status}: {}", truncate_body(&body)),
                }
            }
            _ => Attempt::Fatal(Error::Llm {
                reason: LlmErrorReason::Http,
                message: format!("HTTP {status}: {}", truncate_body(&body)),
            }),
        }
    }

    /// Computed delay before retry number `retry` (0-based):
    /// `base_delay · 2^retry`, capped at `max_delay`, less up to 25% jitter.
    fn backoff(&self, retry: u32) -> Duration {
        let exponential = self
            .retry
            .base_delay
            .saturating_mul(2u32.saturating_pow(retry))
            .min(self.retry.max_delay);
        // RandomState is seeded randomly per instance: a dependency-free
        // source of jitter, which only needs to de-synchronize clients.
        let bits = RandomState::new().hash_one(retry);
        let unit = (bits >> 11) as f64 / (1u64 << 53) as f64; // in [0, 1)
        exponential.mul_f64(1.0 - 0.25 * unit)
    }
}

impl LlmClient for OpenAiCompatClient {
    fn complete_json<'a>(
        &'a self,
        req: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<RawResponse, Error>> {
        self.complete(req).boxed()
    }
}

/// One of the message-keyed HTTP 400 fallbacks, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    OmitTemperature,
    LegacyMaxTokens,
    JsonObject,
}

/// Which fallbacks shape an outgoing body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Downgrades {
    omit_temperature: bool,
    legacy_max_tokens: bool,
    json_object: bool,
}

impl Downgrades {
    fn union(self, other: Downgrades) -> Downgrades {
        Downgrades {
            omit_temperature: self.omit_temperature || other.omit_temperature,
            legacy_max_tokens: self.legacy_max_tokens || other.legacy_max_tokens,
            json_object: self.json_object || other.json_object,
        }
    }

    fn set(&mut self, fallback: Fallback) {
        match fallback {
            Fallback::OmitTemperature => self.omit_temperature = true,
            Fallback::LegacyMaxTokens => self.legacy_max_tokens = true,
            Fallback::JsonObject => self.json_object = true,
        }
    }
}

/// The classified outcome of one HTTP attempt.
enum Attempt {
    /// A usable 2xx response.
    Done(RawResponse),
    /// HTTP 429, HTTP 5xx, a connection error, or a timeout.
    Retryable {
        /// Whether the failure was a timeout.
        timeout: bool,
        /// A server-requested delay overriding the computed backoff.
        retry_after: Option<Duration>,
        /// Description for the exhaustion message.
        detail: String,
    },
    /// HTTP 400 with its raw body, for fallback inspection.
    BadRequest(String),
    /// Anything else: fails the request immediately.
    Fatal(Error),
}

/// The JSON body actually sent: [`ChatRequest`]'s fields in the same order,
/// shaped by the fallbacks in effect.
#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    response_format: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

/// The `response_format` sent once the `json_object` fallback is in effect.
fn json_object_format() -> serde_json::Value {
    serde_json::json!({ "type": "json_object" })
}

/// Serializes the outgoing body for `req` under `downgrades`.
fn encode_body(req: &ChatRequest, downgrades: Downgrades) -> Result<Vec<u8>, Error> {
    let json_object = json_object_format();
    let wire = WireRequest {
        model: &req.model,
        messages: &req.messages,
        response_format: if downgrades.json_object {
            &json_object
        } else {
            &req.response_format
        },
        temperature: req.temperature.filter(|_| !downgrades.omit_temperature),
        max_completion_tokens: (!downgrades.legacy_max_tokens).then_some(req.max_completion_tokens),
        max_tokens: downgrades
            .legacy_max_tokens
            .then_some(req.max_completion_tokens),
    };
    serde_json::to_vec(&wire)
        .map_err(|err| Error::Invariant(format!("cannot serialize a chat request: {err}")))
}

/// Picks the fallback a 400 body calls for: the first, in precedence order,
/// that the (lowercased) body mentions and that is not already in effect
/// for this request. `None` means the 400 is final.
fn choose_fallback(body: &str, req: &ChatRequest, downgrades: Downgrades) -> Option<Fallback> {
    let body = body.to_lowercase();
    if body.contains("temperature") && req.temperature.is_some() && !downgrades.omit_temperature {
        return Some(Fallback::OmitTemperature);
    }
    if body.contains("max_completion_tokens") && !downgrades.legacy_max_tokens {
        return Some(Fallback::LegacyMaxTokens);
    }
    let mentions_schema = ["response_format", "json_schema", "strict"]
        .iter()
        .any(|needle| body.contains(needle));
    if mentions_schema && !downgrades.json_object && req.response_format != json_object_format() {
        return Some(Fallback::JsonObject);
    }
    None
}

/// Classifies a reqwest error: timeouts and connection errors are
/// retryable; anything else is a non-retryable `Http` failure.
fn classify_transport_error(err: &reqwest::Error) -> Attempt {
    let detail = error_chain(err);
    if err.is_timeout() {
        Attempt::Retryable {
            timeout: true,
            retry_after: None,
            detail: format!("timeout: {detail}"),
        }
    } else if err.is_connect() {
        Attempt::Retryable {
            timeout: false,
            retry_after: None,
            detail: format!("connection error: {detail}"),
        }
    } else {
        Attempt::Fatal(Error::Llm {
            reason: LlmErrorReason::Http,
            message: format!("request failed: {detail}"),
        })
    }
}

/// A server-requested retry delay: `retry-after-ms` (milliseconds) wins
/// over `retry-after` (seconds). The HTTP-date form of `retry-after` is not
/// honored; the computed backoff applies instead.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| -> Option<f64> {
        let value: f64 = headers.get(name)?.to_str().ok()?.trim().parse().ok()?;
        (value.is_finite() && value >= 0.0).then_some(value)
    };
    number("retry-after-ms")
        .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
        .or_else(|| number("retry-after").and_then(|s| Duration::try_from_secs_f64(s).ok()))
}

/// The lenient response envelope: only the fields the tool reads; every
/// other field (`id`, `usage`, `system_fingerprint`, …) is ignored.
#[derive(Deserialize)]
struct Envelope {
    choices: Vec<EnvelopeChoice>,
}

#[derive(Deserialize)]
struct EnvelopeChoice {
    message: EnvelopeMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct EnvelopeMessage {
    content: Option<String>,
    refusal: Option<String>,
}

/// Maps a 2xx body to `choices[0]`; an unparseable envelope or one without
/// choices is a non-retryable `Http` failure.
fn parse_envelope(text: &str) -> Attempt {
    let unusable = |why: String| {
        Attempt::Fatal(Error::Llm {
            reason: LlmErrorReason::Http,
            message: format!(
                "unusable response envelope ({why}): {}",
                truncate_body(text)
            ),
        })
    };
    let envelope: Envelope = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(err) => return unusable(err.to_string()),
    };
    match envelope.choices.into_iter().next() {
        Some(choice) => Attempt::Done(RawResponse {
            content: choice.message.content,
            finish_reason: choice.finish_reason,
            refusal: choice.message.refusal,
        }),
        None => unusable("no choices".into()),
    }
}

/// `body`, cut to at most [`MAX_BODY_IN_MESSAGE`] bytes on a character
/// boundary.
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_BODY_IN_MESSAGE {
        return body.to_string();
    }
    let mut end = MAX_BODY_IN_MESSAGE;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &body[..end])
}

/// An error's `Display` followed by its source chain.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with(retry: RetryPolicy) -> OpenAiCompatClient {
        OpenAiCompatClient::new(OpenAiCompatConfig {
            retry,
            ..OpenAiCompatConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn backoff_is_exponential_capped_and_jittered_by_at_most_a_quarter() {
        let client = client_with(RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        });
        let expected_ms = [500u64, 1000, 2000, 4000, 8000, 8000, 8000, 8000];
        for (retry, &full) in expected_ms.iter().enumerate() {
            for _ in 0..50 {
                let delay = client.backoff(retry as u32).as_secs_f64() * 1000.0;
                let full = full as f64;
                assert!(
                    delay <= full && delay >= 0.75 * full,
                    "retry {retry}: {delay} ms"
                );
            }
        }
        // Huge retry counts saturate instead of overflowing.
        assert!(client.backoff(u32::MAX) <= Duration::from_secs(8));
    }

    #[test]
    fn zero_delays_never_wait() {
        let client = client_with(RetryPolicy {
            max_retries: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        });
        assert_eq!(client.backoff(0), Duration::ZERO);
        assert_eq!(client.backoff(5), Duration::ZERO);
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn retry_after_headers_are_parsed() {
        assert_eq!(retry_after(&headers(&[])), None);
        assert_eq!(
            retry_after(&headers(&[("retry-after-ms", "250")])),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "2")])),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "1.5")])),
            Some(Duration::from_millis(1500))
        );
        // Milliseconds win over seconds.
        assert_eq!(
            retry_after(&headers(&[("retry-after-ms", "10"), ("retry-after", "3")])),
            Some(Duration::from_millis(10))
        );
        // Unusable values fall through: HTTP-date, negative, garbage.
        assert_eq!(
            retry_after(&headers(&[(
                "retry-after",
                "Wed, 21 Oct 2015 07:28:00 GMT"
            )])),
            None
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "-1")])), None);
        assert_eq!(
            retry_after(&headers(&[
                ("retry-after-ms", "soon"),
                ("retry-after", "1")
            ])),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn truncate_body_respects_char_boundaries() {
        assert_eq!(truncate_body("short"), "short");
        let exact = "a".repeat(MAX_BODY_IN_MESSAGE);
        assert_eq!(truncate_body(&exact), exact);
        // A 3-byte char straddling the limit is dropped whole.
        let straddling = format!("{}€tail", "a".repeat(MAX_BODY_IN_MESSAGE - 1));
        let cut = truncate_body(&straddling);
        assert!(cut.starts_with(&"a".repeat(MAX_BODY_IN_MESSAGE - 1)));
        assert!(!cut.contains('€') && cut.ends_with("(truncated)"));
    }
}
