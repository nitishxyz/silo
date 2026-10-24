use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Error classes the harness branches on (docs/OTTO.md §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Overloaded,
    RateLimited,
    /// The account's quota, plan or credits are used up: retrying soon cannot help.
    QuotaExhausted,
    ContextOverflow,
    Auth,
    InvalidRequest,
    Network,
    /// No event within the idle timeout.
    Timeout,
    Cancelled,
    /// 5xx other than overload.
    Server,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[error("{class:?}: {message}")]
pub struct LlmError {
    pub class: ErrorClass,
    pub message: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default, with = "opt_millis")]
    #[schemars(with = "Option<u64>")]
    pub retry_after: Option<Duration>,
}

impl LlmError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self { class, message: message.into(), status: None, retry_after: None }
    }
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }
    #[must_use]
    pub fn with_retry_after(mut self, after: Option<Duration>) -> Self {
        self.retry_after = after;
        self
    }
    /// Worth retrying the same request.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self.class, ErrorClass::Overloaded | ErrorClass::RateLimited | ErrorClass::Network | ErrorClass::Timeout | ErrorClass::Server)
    }
    /// The error in words for a person: the provider's message after a short
    /// lead (`Anthropic usage limit reached — resets in 3h 12m`). `provider` is
    /// the display name; `resets_in` stands in when the error carries no wait.
    #[must_use]
    pub fn describe(&self, provider: &str, resets_in: Option<Duration>) -> String {
        let lead = match self.class {
            ErrorClass::QuotaExhausted => {
                let wait = self.retry_after.or(resets_in).filter(|d| !d.is_zero());
                match wait {
                    Some(wait) => format!("{provider} usage limit reached — resets in {}", human_duration(wait)),
                    None => format!("{provider} usage limit reached"),
                }
            }
            ErrorClass::RateLimited => format!("{provider} rate limit hit; try again shortly"),
            ErrorClass::Overloaded => format!("{provider} is overloaded; try again shortly"),
            ErrorClass::Server => format!("{provider} server error"),
            ErrorClass::Network => "Network error".to_owned(),
            ErrorClass::Timeout => format!("{provider} did not respond in time"),
            _ => return format!("{self}"),
        };
        let message = self.message.trim();
        if message.is_empty() { lead } else { format!("{lead}\n{message}") }
    }
}

/// `3h 12m`, `2d 4h`, `45s`: the two largest units.
#[must_use]
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs().max(1);
    let (days, hours, minutes, seconds) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, m) if seconds == 0 => format!("{m}m"),
        (0, 0, m) => format!("{m}m {seconds}s"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

mod opt_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&(d.as_millis() as u64)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_is_terminal_and_described() {
        let quota =
            LlmError::new(ErrorClass::QuotaExhausted, "You've hit your limit").with_retry_after(Some(Duration::from_secs(3 * 3600 + 12 * 60)));
        assert!(!quota.retryable());
        assert_eq!(quota.describe("Claude", None), "Claude usage limit reached — resets in 3h 12m\nYou've hit your limit");
        let unknown = LlmError::new(ErrorClass::QuotaExhausted, "");
        assert_eq!(unknown.describe("OpenAI", None), "OpenAI usage limit reached");
        assert_eq!(unknown.describe("OpenAI", Some(Duration::from_secs(2 * 86_400 + 5 * 3600))), "OpenAI usage limit reached — resets in 2d 5h");
        assert_eq!(serde_json::to_value(ErrorClass::QuotaExhausted).unwrap(), "quota_exhausted");
        assert_eq!(LlmError::new(ErrorClass::Auth, "no").describe("X", None), "Auth: no");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(human_duration(Duration::from_secs(7200)), "2h");
    }
}
