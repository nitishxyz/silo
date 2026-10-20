//! Per-stage timestamps of the keystroke path: the `silo::latency` trace
//! events of client and server (one process in the bench), recorded with
//! the instant they fired.

use std::{
    sync::{Mutex, Once},
    time::{Duration, Instant},
};

use silo_client::proto::kind;
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, filter::filter_fn, layer::Context, prelude::*};

/// Stages in path order; `kind` restricts a frame stage to that frame kind.
pub const STAGES: &[(&str, Option<u8>)] = &[
    ("client_sent", Some(kind::INPUT)),
    ("server_input", None),
    ("actor_input", None),
    ("pty_output", None),
    ("server_output", None),
    ("server_flushed", Some(kind::OUTPUT)),
    ("client_received", Some(kind::OUTPUT)),
];

/// The stages before the PTY: still attributable to one key while another
/// session streams output.
pub const INPUT_STAGES: usize = 3;

struct Stamp {
    at: Instant,
    stage: &'static str,
    kind: Option<u8>,
}

static LOG: Mutex<Vec<Stamp>> = Mutex::new(Vec::new());

/// Start recording (once per process).
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let layer = Recorder.with_filter(filter_fn(|meta| meta.target() == "silo::latency"));
        // A test harness may have installed a subscriber already; the
        // stages are then simply not recorded.
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer));
    });
}

/// Position to read stamps from.
pub fn mark() -> usize {
    LOG.lock().unwrap().len()
}

/// The first stamp of every stage in `stages` since `from`, as offsets from
/// `sent`, up to `until`.
pub fn since(from: usize, sent: Instant, until: Instant, stages: &[(&'static str, Option<u8>)]) -> Vec<Option<Duration>> {
    let log = LOG.lock().unwrap();
    let stamps = log.get(from..).unwrap_or_default();
    stages
        .iter()
        .map(|(stage, kind)| {
            stamps
                .iter()
                .find(|s| s.stage == *stage && s.at <= until && kind.is_none_or(|k| s.kind == Some(k)))
                .map(|s| s.at.saturating_duration_since(sent))
        })
        .collect()
}

/// Drop recorded stamps (between sessions).
pub fn clear() {
    LOG.lock().unwrap().clear();
}

struct Recorder;

#[derive(Default)]
struct Fields {
    stage: Option<&'static str>,
    kind: Option<u8>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "stage" {
            self.stage = STAGES.iter().map(|(s, _)| *s).find(|s| *s == value);
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "kind" {
            self.kind = u8::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let at = Instant::now();
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(stage) = fields.stage {
            LOG.lock().unwrap().push(Stamp { at, stage, kind: fields.kind });
        }
    }
}
