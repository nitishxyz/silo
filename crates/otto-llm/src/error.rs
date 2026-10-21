use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Error classes the harness branches on (docs/OTTO.md §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Overloaded,
    RateLimited,
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
