//! Shared pre-stream retries and cancellable transport.
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use rand::Rng;
use reqwest::{Response, StatusCode, header::HeaderMap};
use tokio_util::sync::CancellationToken;

use crate::{CallCx, ErrorClass, LlmError, RetryNotice};

/// A 429 that asks to wait longer than this is a spent quota or plan, not a throttle.
pub const QUOTA_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct HttpClient(pub reqwest::Client);
impl HttpClient {
    pub fn new() -> Result<Self, LlmError> {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("otto/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(Self)
            .map_err(network)
    }
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_delay: Duration,
    pub max_delay: Duration,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(30) }
    }
}
impl RetryPolicy {
    /// The wait before retrying after failed attempt `attempt` (0-based): the
    /// server's `retry_after`, else jittered backoff, both within `max_delay`.
    /// `None` when the server asks for longer than `max_delay`: give up now
    /// rather than hold the turn silently.
    #[must_use]
    pub fn delay(&self, attempt: usize, error: &LlmError) -> Option<Duration> {
        if let Some(delay) = error.retry_after {
            return (delay <= self.max_delay).then_some(delay);
        }
        let cap = self.base_delay.saturating_mul(1u32.checked_shl(attempt.min(31) as u32).unwrap_or(u32::MAX)).min(self.max_delay);
        Some(cap.mul_f64(rand::rng().random_range(0.5..=1.0)))
    }
}

/// After failed attempt `attempt` (0-based): `Err(error)` when it should not be
/// retried (not retryable, out of attempts, or the server wants too long a
/// wait), else tells `cx.retry` and waits.
///
/// # Errors
/// The error itself, or `Cancelled`.
pub async fn wait_to_retry(policy: &RetryPolicy, attempt: usize, error: LlmError, cx: &CallCx) -> Result<(), LlmError> {
    let max = policy.max_attempts.max(1);
    if !error.retryable() || attempt + 1 >= max {
        return Err(error);
    }
    let Some(delay) = policy.delay(attempt, &error) else { return Err(error) };
    if let Some(hook) = &cx.retry {
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        hook.notify(&RetryNotice { class: error.class, message: error.message, delay, attempt: count(attempt + 2), max_attempts: count(max) });
    }
    tokio::select! {
        biased;
        () = cx.cancel.cancelled() => Err(cancelled()),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

pub(crate) fn network(error: reqwest::Error) -> LlmError {
    LlmError::new(ErrorClass::Network, error.to_string())
}
fn cancelled() -> LlmError {
    LlmError::new(ErrorClass::Cancelled, "request cancelled")
}

#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let now = SystemTime::now();
    if let Some(ms) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
        return Some(Duration::from_millis(ms));
    }
    if let Some(value) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        if let Ok(date) = httpdate::parse_http_date(value) {
            return Some(date.duration_since(now).unwrap_or_default());
        }
    }
    // Window resets: the windows that are spent (`…remaining…: 0`, `…status: rejected`)
    // when any say so, else every window. The longest decides.
    let resets: Vec<(bool, Duration)> = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let spent = if let Some(window) = name.strip_prefix("x-ratelimit-reset") {
                header_is(headers, &format!("x-ratelimit-remaining{window}"), "0")
            } else {
                let window = name.strip_prefix("anthropic-ratelimit-").and_then(|n| n.strip_suffix("-reset"))?;
                header_is(headers, &format!("anthropic-ratelimit-{window}-remaining"), "0")
                    || header_is(headers, &format!("anthropic-ratelimit-{window}-status"), "rejected")
            };
            Some((spent, reset_in(value.to_str().ok()?, now)?))
        })
        .collect();
    let any_spent = resets.iter().any(|(spent, _)| *spent);
    resets.into_iter().filter(|(spent, _)| *spent || !any_spent).map(|(_, d)| d).max()
}

fn header_is(headers: &HeaderMap, name: &str, value: &str) -> bool {
    headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
}

/// A reset as an RFC 3339 time, a Unix epoch (s or ms), or a duration (`1m2.5s`).
fn reset_in(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        let time = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(u64::try_from(date.timestamp_millis()).ok()?))?;
        return Some(time.duration_since(now).unwrap_or_default());
    }
    if let Ok(epoch) = value.parse::<u64>() {
        let time =
            SystemTime::UNIX_EPOCH.checked_add(if epoch > 10_000_000_000 { Duration::from_millis(epoch) } else { Duration::from_secs(epoch) })?;
        return Some(time.duration_since(now).unwrap_or_default());
    }
    parse_duration(value)
}

/// The wait an error body names: Codex's `resets_in_seconds` / `resets_at`, Google's
/// `RetryInfo.retryDelay`, or a message's `reset after 2h31m10s` / `resets in 3h`.
#[must_use]
pub fn body_retry_after(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let error = if value["error"].is_object() { &value["error"] } else { &value };
    if let Some(seconds) = error["resets_in_seconds"].as_u64() {
        return Some(Duration::from_secs(seconds));
    }
    if let Some(at) = error["resets_at"].as_u64() {
        return reset_in(&at.to_string(), SystemTime::now());
    }
    let delay = error["details"].as_array().into_iter().flatten().find_map(|d| d["retryDelay"].as_str()).and_then(parse_duration);
    if delay.is_some() {
        return delay;
    }
    let text = body.to_ascii_lowercase();
    ["reset after ", "resets in ", "reset in ", "try again in "].iter().find_map(|lead| {
        let rest = &text[text.find(lead)? + lead.len()..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | 'h' | 'm' | 's'))).unwrap_or(rest.len());
        parse_duration(rest[..end].trim_end_matches('.')).filter(|d| !d.is_zero())
    })
}

fn parse_duration(mut value: &str) -> Option<Duration> {
    let mut seconds = 0.0;
    while !value.is_empty() {
        let n = value.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let number: f64 = value[..n].parse().ok()?;
        value = &value[n..];
        let (unit, scale) = if value.starts_with("ms") {
            (2, 0.001)
        } else if value.starts_with('s') {
            (1, 1.0)
        } else if value.starts_with('m') {
            (1, 60.0)
        } else if value.starts_with('h') {
            (1, 3600.0)
        } else {
            return None;
        };
        seconds += number * scale;
        value = &value[unit..];
    }
    Duration::try_from_secs_f64(seconds).ok()
}

/// Error-body words that mean the account's quota, plan or credits are used up
/// (any 4xx): OpenAI/compat `insufficient_quota`, Codex `usage_limit_reached`,
/// Claude's usage limit and `billing_error`/credit balance, Moonshot/Kimi
/// `exceeded_current_quota_error` and balance, xAI credits/spending limit,
/// Antigravity's individual quota, DeepSeek/OpenRouter balance and credits.
const SPENT: &[&str] = &[
    "insufficient_quota",
    "usage_limit_reached",
    "usage limit",
    "billing_error",
    "credit balance",
    "insufficient balance",
    "insufficient_balance",
    "insufficient credits",
    "out of credits",
    "available credits",
    "spending limit",
    "exceeded_current_quota",
    "individual quota reached",
    "quota_exhausted",
];
/// Words that mean a quota when no short wait says it is a throttle (Gemini says
/// `RESOURCE_EXHAUSTED` / "exceeded your current quota" for per-minute limits too).
const QUOTA: &[&str] = &["quota", "resource_exhausted", "resource exhausted"];

/// The class of an HTTP error from its status, the body's words, and the wait it names.
#[must_use]
pub fn classify(status: u16, body: &str, wait: Option<Duration>) -> ErrorClass {
    let text = body.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if status == 400 && has(&["prompt is too long", "context_length_exceeded", "maximum context length"]) {
        return ErrorClass::ContextOverflow;
    }
    let short_wait = wait.is_some_and(|w| w <= QUOTA_WAIT);
    let quota = status == 402
        || ((400..500).contains(&status) && has(SPENT))
        || (status == 429 && ((has(QUOTA) && !short_wait) || wait.is_some_and(|w| w > QUOTA_WAIT)));
    if quota {
        return ErrorClass::QuotaExhausted;
    }
    match status {
        429 => ErrorClass::RateLimited,
        529 | 503 => ErrorClass::Overloaded,
        401 | 403 => ErrorClass::Auth,
        400..=499 => ErrorClass::InvalidRequest,
        500..=599 => ErrorClass::Server,
        _ => ErrorClass::Other,
    }
}

#[must_use]
pub fn classify_status(status: StatusCode, headers: &HeaderMap, body: &str) -> LlmError {
    let wait = retry_after(headers).or_else(|| body_retry_after(body));
    LlmError::new(classify(status.as_u16(), body, wait), body).with_status(status.as_u16()).with_retry_after(wait)
}

/// The provider's `message` from an error body (`{"error":{"message"}}`, `{"message"}` or `{"error":"..."}`), else the body.
#[must_use]
pub fn provider_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else { return body.trim().to_owned() };
    let text = |v: &serde_json::Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    text(&value["error"]["message"])
        .or_else(|| text(&value["message"]))
        .or_else(|| text(&value["error"]).map(|e| text(&value["error_description"]).map_or_else(|| e.clone(), |d| format!("{e}: {d}"))))
        .unwrap_or_else(|| body.trim().to_owned())
}

/// Send, retrying retryable failures within `policy` (each wait reported to `cx.retry`).
///
/// # Errors
/// The last failure, classified; `Cancelled` once `cx.cancel` fires.
pub async fn send_with_retry<F>(mut build_request: F, policy: &RetryPolicy, cx: &CallCx) -> Result<Response, LlmError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let cancel = &cx.cancel;
    for attempt in 0..policy.max_attempts.max(1) {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(cancelled()),
            result = build_request().send() => result,
        };
        let error = match result {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                let body = tokio::select! {
                    _ = cancel.cancelled() => return Err(cancelled()),
                    body = tokio::time::timeout(Duration::from_secs(30), response.text()) => match body {
                        Ok(body) => body.map_err(network)?,
                        Err(_) => return Err(LlmError::new(ErrorClass::Timeout, "error response body timed out")),
                    },
                };
                LlmError { message: provider_message(&body), ..classify_status(status, &headers, &body) }
            }
            Err(error) => network(error),
        };
        wait_to_retry(policy, attempt, error, cx).await?;
    }
    unreachable!()
}

pub fn byte_stream_with_idle_timeout(
    response: Response,
    idle: Duration,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<Bytes, LlmError>> + Send {
    idle_stream(response.bytes_stream().map(|r| r.map_err(network)), idle, cancel)
}

fn idle_stream<S>(stream: S, idle: Duration, cancel: CancellationToken) -> impl Stream<Item = Result<Bytes, LlmError>> + Send
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    futures::stream::unfold((Some(Box::pin(stream)), cancel), move |(stream, cancel)| async move {
        let mut stream = stream?;
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(cancelled()),
            result = tokio::time::timeout(idle, stream.next()) => match result {
                Ok(Some(item)) => item,
                Ok(None) => return None,
                Err(_) => Err(LlmError::new(ErrorClass::Timeout, "stream idle timeout")),
            },
        };
        let next = if item.is_err() { None } else { Some(stream) };
        Some((item, (next, cancel)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classification_and_headers() {
        for (code, body, class) in [
            (429, "", ErrorClass::RateLimited),
            (529, "", ErrorClass::Overloaded),
            (503, "", ErrorClass::Overloaded),
            (401, "", ErrorClass::Auth),
            (403, "", ErrorClass::Auth),
            (400, "prompt is too long", ErrorClass::ContextOverflow),
            (400, "context_length_exceeded", ErrorClass::ContextOverflow),
            (400, "maximum context length", ErrorClass::ContextOverflow),
            (422, "", ErrorClass::InvalidRequest),
            (500, "", ErrorClass::Server),
        ] {
            assert_eq!(classify_status(StatusCode::from_u16(code).unwrap(), &HeaderMap::new(), body).class, class);
        }
        let anthropic =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"tools.24.custom.input_schema: bad"},"request_id":"req_1"}"#;
        assert_eq!(provider_message(anthropic), "tools.24.custom.input_schema: bad");
        assert_eq!(provider_message(r#"{"error":{"message":"Invalid schema","type":"invalid_request_error","code":null}}"#), "Invalid schema");
        assert_eq!(provider_message(r#"{"error":"invalid_grant","error_description":"expired"}"#), "invalid_grant: expired");
        assert_eq!(provider_message(" upstream down \n"), "upstream down");
        assert_eq!(provider_message("{}"), "{}");
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset-tokens", "1m2.5s".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(62500)));
        headers.insert("retry-after", "3".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
        headers.insert("retry-after-ms", "17".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(17)));
    }
    #[test]
    fn reset_dates_and_backoff() {
        let future = SystemTime::now() + Duration::from_secs(60);
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", httpdate::fmt_http_date(future).parse().unwrap());
        assert!(retry_after(&headers).unwrap() > Duration::from_secs(58));
        headers.clear();
        headers.insert("anthropic-ratelimit-tokens-reset", "2999-01-01T00:00:00Z".parse().unwrap());
        assert!(retry_after(&headers).unwrap() > Duration::from_secs(60));
        let policy = RetryPolicy::default();
        let error = LlmError::new(ErrorClass::Network, "test");
        for attempt in 0..100 {
            assert!(policy.delay(attempt, &error).unwrap() <= policy.max_delay);
        }
        assert_eq!(policy.delay(0, &error.clone().with_retry_after(Some(Duration::from_secs(30)))), Some(Duration::from_secs(30)));
        // Longer than the cap: no retry at all, rather than a silent hours-long sleep.
        assert_eq!(policy.delay(0, &error.with_retry_after(Some(Duration::from_secs(90)))), None);
    }

    #[tokio::test]
    async fn cancelled_send_never_connects() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let client = HttpClient::new().unwrap();
        let cx = CallCx { cancel, ..CallCx::default() };
        let error = send_with_retry(|| client.client().get("http://127.0.0.1:1"), &RetryPolicy::default(), &cx).await.unwrap_err();
        assert_eq!(error.class, ErrorClass::Cancelled);
    }

    #[tokio::test]
    async fn idle_and_cancel_are_terminal() {
        let cancel = CancellationToken::new();
        let stream = idle_stream(futures::stream::pending(), Duration::from_millis(1), cancel.clone());
        let result: Vec<_> = stream.collect().await;
        assert_eq!(result[0].as_ref().unwrap_err().class, ErrorClass::Timeout);
        cancel.cancel();
        let result: Vec<_> = idle_stream(futures::stream::pending(), Duration::from_secs(10), cancel).collect().await;
        assert_eq!(result[0].as_ref().unwrap_err().class, ErrorClass::Cancelled);
    }

    #[test]
    fn quota_bodies_per_provider() {
        let h = HeaderMap::new();
        let class = |code: u16, body: &str| classify_status(StatusCode::from_u16(code).unwrap(), &h, body).class;
        // OpenAI (and compat dialects echoing it): insufficient_quota is billing, not a throttle.
        let openai = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert_eq!(class(429, openai), ErrorClass::QuotaExhausted);
        let tpm = r#"{"error":{"message":"Rate limit reached for gpt-5 on tokens per min (TPM). Please try again in 1.5s.","type":"tokens","code":"rate_limit_exceeded"}}"#;
        assert_eq!(class(429, tpm), ErrorClass::RateLimited);
        // Codex (ChatGPT plan): the window's reset rides in the body.
        let codex =
            r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_in_seconds":11520}}"#;
        let e = classify_status(StatusCode::TOO_MANY_REQUESTS, &h, codex);
        assert_eq!((e.class, e.retry_after), (ErrorClass::QuotaExhausted, Some(Duration::from_secs(11520))));
        // Anthropic: credit balance (400 billing_error); subscription limit by its spent unified window.
        let billing =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        assert_eq!(class(400, billing), ErrorClass::QuotaExhausted);
        let limited = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit."}}"#;
        assert_eq!(class(429, limited), ErrorClass::RateLimited);
        let mut unified = HeaderMap::new();
        let reset = (SystemTime::now() + Duration::from_secs(3 * 3600)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        unified.insert("anthropic-ratelimit-unified-5h-status", "rejected".parse().unwrap());
        unified.insert("anthropic-ratelimit-unified-5h-reset", reset.to_string().parse().unwrap());
        let e = classify_status(StatusCode::TOO_MANY_REQUESTS, &unified, limited);
        assert_eq!(e.class, ErrorClass::QuotaExhausted);
        assert!(e.retry_after.unwrap() > Duration::from_secs(3 * 3600 - 10));
        // Gemini: RESOURCE_EXHAUSTED with a short RetryInfo is a per-minute throttle; without one, the quota.
        let gemini = |delay: &str| {
            format!(
                r#"{{"error":{{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.","status":"RESOURCE_EXHAUSTED","details":[{{"@type":"type.googleapis.com/google.rpc.RetryInfo"{delay}}}]}}}}"#
            )
        };
        assert_eq!(class(429, &gemini(r#","retryDelay":"34s""#)), ErrorClass::RateLimited);
        assert_eq!(class(429, &gemini("")), ErrorClass::QuotaExhausted);
        assert_eq!(class(429, &gemini(r#","retryDelay":"3600s""#)), ErrorClass::QuotaExhausted);
        // Antigravity: the account's model quota, reset named in the message.
        let antigravity = r#"{"error":{"code":429,"message":"You have exhausted your capacity on this model. Your quota will reset after 2h31m10s.","status":"RESOURCE_EXHAUSTED"}}"#;
        let e = classify_status(StatusCode::TOO_MANY_REQUESTS, &h, antigravity);
        assert_eq!((e.class, e.retry_after), (ErrorClass::QuotaExhausted, Some(Duration::from_secs(2 * 3600 + 31 * 60 + 10))));
        // Moonshot / Kimi Code.
        let kimi = r#"{"error":{"message":"Your account is suspended due to insufficient balance","type":"exceeded_current_quota_error"}}"#;
        assert_eq!(class(429, kimi), ErrorClass::QuotaExhausted);
        assert_eq!(class(403, r#"{"error":{"message":"You've reached your usage limit for this period"}}"#), ErrorClass::QuotaExhausted);
        assert_eq!(class(429, r#"{"error":{"type":"engine_overloaded_error","message":"busy"}}"#), ErrorClass::RateLimited);
        // xAI credits, DeepSeek/OpenRouter 402.
        let xai = r#"{"code":"Some resource has been exhausted","error":"Your team has either used all available credits or reached its monthly spending limit."}"#;
        assert_eq!(class(403, xai), ErrorClass::QuotaExhausted);
        assert_eq!(class(402, r#"{"error":{"message":"Insufficient Balance"}}"#), ErrorClass::QuotaExhausted);
        // Any 429 asking for more than a minute.
        let mut long = HeaderMap::new();
        long.insert("retry-after", "600".parse().unwrap());
        assert_eq!(classify_status(StatusCode::TOO_MANY_REQUESTS, &long, "").class, ErrorClass::QuotaExhausted);
        long.insert("retry-after", "20".parse().unwrap());
        assert_eq!(classify_status(StatusCode::TOO_MANY_REQUESTS, &long, "").class, ErrorClass::RateLimited);
        assert_eq!(class(400, "bad tool schema"), ErrorClass::InvalidRequest);
        assert_eq!(class(500, "quota service down"), ErrorClass::Server);
    }

    #[test]
    fn spent_windows_decide_the_wait() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "6m0s".parse().unwrap());
        headers.insert("x-ratelimit-remaining-requests", "40".parse().unwrap());
        headers.insert("x-ratelimit-reset-tokens", "2s".parse().unwrap());
        headers.insert("x-ratelimit-remaining-tokens", "0".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
        headers.remove("x-ratelimit-remaining-tokens");
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(360)));
        assert_eq!(body_retry_after(r#"{"error":{"message":"Please try again in 20s."}}"#), Some(Duration::from_secs(20)));
        assert_eq!(body_retry_after("{}"), None);
    }

    /// Serves `replies` (status, extra headers, body) one per connection; counts requests.
    async fn serve(replies: Vec<(u16, &'static str, &'static str)>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            for (status, headers, body) in replies {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let mut buf = vec![0; 8192];
                let _ = socket.read(&mut buf).await;
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = format!("HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, count)
    }

    #[tokio::test]
    async fn long_waits_fail_at_once_and_short_ones_are_announced() {
        let client = HttpClient::new().unwrap();
        let cx = CallCx::default();
        // An exhausted plan: one request, then the error, not a sleep until the reset.
        let (url, count) = serve(vec![(429, "retry-after: 18000\r\n", r#"{"error":{"message":"limit"}}"#)]).await;
        let started = std::time::Instant::now();
        let error = send_with_retry(|| client.client().get(&url), &RetryPolicy::default(), &cx).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!((error.class, error.message.as_str(), count.load(std::sync::atomic::Ordering::SeqCst)), (ErrorClass::QuotaExhausted, "limit", 1));
        // A 429 under the cap but past max_delay is not retried either.
        let policy = RetryPolicy { max_delay: Duration::from_secs(1), ..RetryPolicy::default() };
        let (url, count) = serve(vec![(429, "retry-after: 20\r\n", "slow down")]).await;
        let error = send_with_retry(|| client.client().get(&url), &policy, &cx).await.unwrap_err();
        assert_eq!((error.class, count.load(std::sync::atomic::Ordering::SeqCst)), (ErrorClass::RateLimited, 1));
        // A short throttle: retried, and each wait reported first.
        let notices = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = notices.clone();
        let cx = CallCx { retry: Some(crate::RetryHook::new(move |n| sink.lock().unwrap().push(n.clone()))), ..CallCx::default() };
        let (url, count) = serve(vec![(429, "retry-after-ms: 10\r\n", "busy"), (200, "", "ok")]).await;
        let response = send_with_retry(|| client.client().get(&url), &RetryPolicy::default(), &cx).await.unwrap();
        assert_eq!((response.status().as_u16(), count.load(std::sync::atomic::Ordering::SeqCst)), (200, 2));
        let notices = notices.lock().unwrap();
        assert_eq!(
            *notices,
            [RetryNotice { class: ErrorClass::RateLimited, message: "busy".into(), delay: Duration::from_millis(10), attempt: 2, max_attempts: 3 }]
        );
    }
}
