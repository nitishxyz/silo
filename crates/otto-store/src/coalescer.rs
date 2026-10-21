use std::time::Duration;

use crate::EventRecord;

/// One session's delta buffer. The caller supplies monotonic elapsed time and
/// appends returned records in order. Only string `payload.text` is concatenated;
/// all other payload fields, kind, turn and step must agree. Drive `flush_due`
/// from a timer even when no new events arrive, and `flush` at shutdown.
#[derive(Debug)]
pub struct Coalescer {
    session_id: String,
    pending: Option<EventRecord>,
    started: Duration,
}

impl Coalescer {
    pub const MAX_DELAY: Duration = Duration::from_millis(50);
    pub const MAX_BYTES: usize = 4096;

    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), pending: None, started: Duration::ZERO }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns records ready to append, including non-deltas without buffering.
    #[must_use]
    pub fn push(&mut self, record: EventRecord, now: Duration) -> Vec<EventRecord> {
        let mut ready = Vec::new();
        if let Some(expired) = self.flush_due(now) {
            ready.push(expired);
        }
        if !is_delta(&record) {
            if let Some(pending) = self.flush() {
                ready.push(pending);
            }
            ready.push(record);
            return ready;
        }
        if self.pending.as_ref().is_some_and(|p| !compatible(p, &record)) {
            ready.extend(self.flush());
        }
        if let Some(pending) = &mut self.pending {
            let mut text = pending.payload["text"].as_str().unwrap().to_owned();
            text.push_str(record.payload["text"].as_str().unwrap());
            pending.payload["text"] = text.into();
        } else {
            self.started = now;
            self.pending = Some(record);
        }
        if self.pending.as_ref().is_some_and(|p| p.payload.to_string().len() >= Self::MAX_BYTES) {
            ready.extend(self.flush());
        }
        ready
    }

    #[must_use]
    pub fn flush_due(&mut self, now: Duration) -> Option<EventRecord> {
        if now.saturating_sub(self.started) >= Self::MAX_DELAY { self.flush() } else { None }
    }

    #[must_use]
    pub fn flush(&mut self) -> Option<EventRecord> {
        self.pending.take()
    }
}

fn is_delta(record: &EventRecord) -> bool {
    matches!(record.kind.as_str(), "text.delta" | "reasoning.delta" | "text_delta" | "reasoning_delta")
        && record.payload.get("text").is_some_and(serde_json::Value::is_string)
}

fn compatible(a: &EventRecord, b: &EventRecord) -> bool {
    if a.turn != b.turn || a.step != b.step || a.kind != b.kind {
        return false;
    }
    let mut a = a.payload.clone();
    let mut b = b.payload.clone();
    a.as_object_mut().unwrap().remove("text");
    b.as_object_mut().unwrap().remove("text");
    a == b
}
