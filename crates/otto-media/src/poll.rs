//! Waiting for an asynchronous generation (videos, Higgsfield's images): poll its status every 2 s growing ×1.5 to
//! 10 s, tell the user how it goes, withdraw it on cancel, give up after a while.
use crate::http::send;
use crate::{Call, MediaError, MediaErrorKind};
use serde_json::Value;
use std::time::{Duration, Instant};

/// How long a video may take.
pub(crate) const VIDEO_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Where a job stands, read from its status.
pub(crate) enum Stage {
    /// Still running; what to show (`queued`, `40 %`, `2 ahead in the queue`), maybe nothing.
    Waiting(String),
    Done(Value),
}

/// A submitted job.
pub(crate) struct Job<'a> {
    pub name: &'a str,
    pub key: &'a str,
    /// The first wait between checks.
    pub first: Duration,
    pub timeout: Duration,
    /// Sent (best effort) when the call is cancelled: the provider's cancel request.
    pub cancel: Option<reqwest::RequestBuilder>,
}

/// Poll `check` from `value` (the submit's response, often its first status) until `stage` says done or fails.
pub(crate) async fn wait(
    call: &Call,
    job: Job<'_>,
    mut value: Value,
    stage: impl Fn(&Value) -> Result<Stage, MediaError>,
    check: impl Fn() -> reqwest::RequestBuilder,
) -> Result<Value, MediaError> {
    let Job { name, key, first, timeout, mut cancel } = job;
    let withdraw = |cancel: Option<reqwest::RequestBuilder>| async move {
        if let Some(cancel) = cancel {
            let _ = cancel.timeout(Duration::from_secs(5)).send().await;
        }
        MediaError::cancelled()
    };
    let (started, mut delay, mut misses) = (Instant::now(), first, 0);
    loop {
        let shown = match stage(&value)? {
            Stage::Done(done) => return Ok(done),
            Stage::Waiting(shown) => shown,
        };
        if started.elapsed() > timeout {
            return Err(MediaError::failed(format!("{name}: no result after {} min", timeout.as_secs() / 60)));
        }
        let secs = started.elapsed().as_secs();
        (call.progress)(if shown.is_empty() {
            format!("Generating with {name}… {secs} s")
        } else {
            format!("Generating with {name}… {shown}, {secs} s")
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = call.cancel.cancelled() => return Err(withdraw(cancel.take()).await),
        }
        delay = delay.mul_f64(1.5).min(Duration::from_secs(10));
        match send(call, name, key, check()).await {
            Ok(next) => (value, misses) = (next, 0),
            // Network blips and 5xx: keep polling, a few times.
            Err(e) if e.kind == MediaErrorKind::Failed && misses < 3 && !e.message.contains("(HTTP 4") => misses += 1,
            Err(e) if e.kind == MediaErrorKind::Cancelled => return Err(withdraw(cancel.take()).await),
            Err(e) => return Err(e),
        }
    }
}
