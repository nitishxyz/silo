//! Shared pre-stream retries and cancellable transport.
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use rand::Rng;
use reqwest::{Response, StatusCode, header::HeaderMap};
use tokio_util::sync::CancellationToken;

use crate::{ErrorClass, LlmError};

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
    #[must_use]
    pub fn delay(&self, attempt: usize, error: &LlmError) -> Duration {
        if let Some(delay) = error.retry_after {
            return delay;
        }
        let cap = self.base_delay.saturating_mul(1u32.checked_shl(attempt.min(31) as u32).unwrap_or(u32::MAX)).min(self.max_delay);
        cap.mul_f64(rand::rng().random_range(0.5..=1.0))
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
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            if !(name.starts_with("x-ratelimit-reset") || (name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset"))) {
                return None;
            }
            let value = value.to_str().ok()?;
            if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
                let time = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(u64::try_from(date.timestamp_millis()).ok()?))?;
                return Some(time.duration_since(now).unwrap_or_default());
            }
            if let Ok(epoch) = value.parse::<u64>() {
                let time = SystemTime::UNIX_EPOCH.checked_add(if epoch > 10_000_000_000 {
                    Duration::from_millis(epoch)
                } else {
                    Duration::from_secs(epoch)
                })?;
                return Some(time.duration_since(now).unwrap_or_default());
            }
            parse_duration(value)
        })
        .max()
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

#[must_use]
pub fn classify_status(status: StatusCode, headers: &HeaderMap, body: &str) -> LlmError {
    let text = body.to_ascii_lowercase();
    let class = match status.as_u16() {
        429 => ErrorClass::RateLimited,
        529 | 503 => ErrorClass::Overloaded,
        401 | 403 => ErrorClass::Auth,
        400 if ["prompt is too long", "context_length_exceeded", "maximum context length"].iter().any(|s| text.contains(s)) => {
            ErrorClass::ContextOverflow
        }
        400..=499 => ErrorClass::InvalidRequest,
        500..=599 => ErrorClass::Server,
        _ => ErrorClass::Other,
    };
    LlmError::new(class, body).with_status(status.as_u16()).with_retry_after(retry_after(headers))
}

pub async fn send_with_retry<F>(mut build_request: F, policy: &RetryPolicy, cancel: &CancellationToken) -> Result<Response, LlmError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
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
                classify_status(status, &headers, &body)
            }
            Err(error) => network(error),
        };
        if !error.retryable() || attempt + 1 >= policy.max_attempts.max(1) {
            return Err(error);
        }
        tokio::select! { _ = cancel.cancelled() => return Err(cancelled()), _ = tokio::time::sleep(policy.delay(attempt, &error)) => {} }
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
            assert!(policy.delay(attempt, &error) <= policy.max_delay);
        }
        assert_eq!(policy.delay(0, &error.with_retry_after(Some(Duration::from_secs(90)))), Duration::from_secs(90));
    }

    #[tokio::test]
    async fn cancelled_send_never_connects() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let client = HttpClient::new().unwrap();
        let error = send_with_retry(|| client.client().get("http://127.0.0.1:1"), &RetryPolicy::default(), &cancel).await.unwrap_err();
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
}
