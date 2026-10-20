//! `SILO_LATENCY_TRACE=<file>` (docs/PHASE5.md §6): one JSON line per key,
//! in send order. `paint_pred_ms` is the time to the end of the first paint
//! that showed it as a prediction, and `paint_echo_ms` the time to the first
//! paint whose grid held the echo. A null time comes with its reason in
//! `no_pred` / `no_echo`. The echo's stages on the way there:
//! `mirror_echo_ms` (applied to the mirror) and `prepared_echo_ms` (its
//! frame prepared for paint).

use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::File,
    io::Write,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{IntoElement, Styled as _, canvas};
use silo_render::{Predicted, PreparedFrame};
use silo_vt::Frame;

/// A key whose echo never shows (wrong cell, no echo) is written without it.
const GIVE_UP: Duration = Duration::from_secs(5);

const NOT_PRINTABLE: &str = "not printable";
const NOT_PREDICTED: &str = "not predicted";
const NEVER_ECHOED: &str = "never echoed";

struct Key {
    seq: u64,
    text: String,
    /// Where a single printable key should appear; `None` for the rest.
    cell: Option<(u16, u16)>,
    at: Instant,
    pred: Option<Instant>,
    mirror: Option<Instant>,
    prepared: Option<Instant>,
    echo: Option<Instant>,
}

/// Where an echo was seen on its way to the screen.
#[derive(Clone, Copy)]
pub enum Stage {
    Mirror,
    Prepared,
}

impl Key {
    fn stage(&self, stage: Stage) -> Option<Instant> {
        match stage {
            Stage::Mirror => self.mirror,
            Stage::Prepared => self.prepared,
        }
    }

    fn stage_mut(&mut self, stage: Stage) -> &mut Option<Instant> {
        match stage {
            Stage::Mirror => &mut self.mirror,
            Stage::Prepared => &mut self.prepared,
        }
    }

    fn done(&self, now: Instant) -> bool {
        self.cell.is_none() || self.echo.is_some() || now - self.at >= GIVE_UP
    }

    fn record(&self) -> String {
        let ms = |t: Option<Instant>| t.map(|t| (t - self.at).as_secs_f64() * 1000.0);
        let reason = |t: Option<Instant>, missing| match (t, self.cell) {
            (Some(_), _) => None,
            (None, None) => Some(NOT_PRINTABLE),
            (None, Some(_)) => Some(missing),
        };
        serde_json::json!({
            "seq": self.seq,
            "key": self.text,
            "paint_pred_ms": ms(self.pred),
            "no_pred": reason(self.pred, NOT_PREDICTED),
            "mirror_echo_ms": ms(self.mirror),
            "prepared_echo_ms": ms(self.prepared),
            "paint_echo_ms": ms(self.echo),
            "no_echo": reason(self.echo, NEVER_ECHOED),
        })
        .to_string()
    }
}

pub struct Trace {
    out: Box<dyn Write>,
    seq: u64,
    keys: VecDeque<Key>,
}

impl Trace {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("SILO_LATENCY_TRACE")?;
        let file = File::options().create(true).append(true).open(&path);
        let file = file.inspect_err(|error| tracing::warn!("latency trace {}: {error}", path.display())).ok()?;
        Some(Self::new(Box::new(file)))
    }

    fn new(out: Box<dyn Write>) -> Self {
        Self { out, seq: 0, keys: VecDeque::new() }
    }

    /// A key's encoded bytes are about to be sent; `cell` is where a single
    /// printable byte should appear.
    pub fn key(&mut self, bytes: &[u8], cell: (u16, u16)) {
        let printable = matches!(bytes, [0x20..=0x7e]);
        self.seq += 1;
        let text = String::from_utf8_lossy(bytes).into_owned();
        let at = Instant::now();
        self.keys.push_back(Key { seq: self.seq, text, cell: printable.then_some(cell), at, pred: None, mirror: None, prepared: None, echo: None });
        self.flush(at);
    }

    /// Whether any key still waits for its echo to reach `stage`.
    pub fn waiting(&self, stage: Stage) -> bool {
        self.keys.iter().any(|k| k.cell.is_some() && k.echo.is_none() && k.stage(stage).is_none())
    }

    /// `frame` reached `stage` now: stamp the keys whose echo it holds.
    pub fn seen(&mut self, stage: Stage, frame: &Frame) {
        let (now, columns) = (Instant::now(), usize::from(frame.size.columns));
        for key in &mut self.keys {
            let Some(cell) = key.cell.filter(|_| key.echo.is_none() && key.stage(stage).is_none()) else { continue };
            let index = usize::from(cell.0) * columns + usize::from(cell.1);
            if frame.cells.get(index).is_some_and(|c| c.text == key.text) {
                *key.stage_mut(stage) = Some(now);
            }
        }
    }

    /// A paint finished at `now` with these predicted cells over `frame`.
    fn painted(&mut self, predicted: &[Predicted], frame: &Frame, now: Instant) {
        let columns = usize::from(frame.size.columns);
        for key in &mut self.keys {
            let Some(cell) = key.cell.filter(|_| key.echo.is_none()) else { continue };
            if key.pred.is_none() && predicted.iter().any(|p| (p.row, p.column) == cell && p.text == key.text) {
                key.pred = Some(now);
            }
            let index = usize::from(cell.0) * columns + usize::from(cell.1);
            if frame.cells.get(index).is_some_and(|c| c.text == key.text) {
                key.echo = Some(now);
            }
        }
        self.flush(now);
    }

    /// Write finished keys from the front, keeping send order.
    fn flush(&mut self, now: Instant) {
        while self.keys.front().is_some_and(|k| k.done(now)) {
            let key = self.keys.pop_front().expect("front");
            self.write(&key);
        }
    }

    fn write(&mut self, key: &Key) {
        if let Err(error) = writeln!(self.out, "{}", key.record()) {
            tracing::warn!("latency trace: {error}");
        }
    }
}

impl Drop for Trace {
    /// Keys still waiting for an echo are written as never echoed.
    fn drop(&mut self) {
        while let Some(key) = self.keys.pop_front() {
            self.write(&key);
        }
        let _ = self.out.flush();
    }
}

/// Painted after the terminal canvas: stamps the end of this frame's paint.
pub fn paint_probe(trace: Rc<RefCell<Trace>>, prepared: Arc<PreparedFrame>, predicted: Vec<Predicted>) -> impl IntoElement {
    canvas(|_, _, _| {}, move |_, (), _, _| trace.borrow_mut().painted(&predicted, prepared.frame(), Instant::now())).absolute().size_full()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use silo_vt::{GridSize, TerminalCore};

    use super::*;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn records(sink: &Sink) -> Vec<serde_json::Value> {
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn one_record_per_key_in_send_order() {
        let sink = Sink::default();
        let mut trace = Trace::new(Box::new(sink.clone()));
        let mut vt = TerminalCore::new(GridSize { columns: 20, rows: 5, cell_width: 8, cell_height: 16 }).unwrap();
        trace.key(b"a", (0, 0));
        trace.key(b"\r", (0, 1));
        trace.key(b"b", (1, 0));
        // `b` is predicted and echoed first; `a` lands elsewhere.
        vt.write_output(b"x\r\nb").unwrap();
        let predicted = [Predicted { row: 1, column: 0, text: "b".into() }];
        trace.painted(&predicted, &vt.frame().unwrap(), Instant::now());
        assert!(records(&sink).is_empty(), "held behind the pending `a`");
        drop(trace);
        let records = records(&sink);
        let keys: Vec<_> = records.iter().map(|r| r["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["a", "\r", "b"]);
        assert_eq!((records[0]["no_pred"].as_str(), records[0]["no_echo"].as_str()), (Some(NOT_PREDICTED), Some(NEVER_ECHOED)));
        assert_eq!(records[1]["no_echo"].as_str(), Some(NOT_PRINTABLE));
        assert!(records[2]["paint_pred_ms"].is_f64() && records[2]["paint_echo_ms"].is_f64());
        assert!(records[2]["no_pred"].is_null() && records[2]["no_echo"].is_null());
    }
}
