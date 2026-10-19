//! `SILO_LATENCY_TRACE=<file>` (docs/PHASE5.md §6): for each printable key,
//! the time from the keystroke to the end of the first paint that showed
//! it as a prediction (`paint_pred_ms`) and as the mirror's echo
//! (`paint_echo_ms`). One JSON line per key, appended to the file.

use std::{
    cell::RefCell,
    fs::File,
    io::Write as _,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{IntoElement, Styled as _, canvas};
use silo_render::{Predicted, PreparedFrame};
use silo_vt::Frame;

/// A key whose echo never shows (wrong cell, no echo) is logged without it.
const GIVE_UP: Duration = Duration::from_secs(5);

struct Key {
    text: String,
    cell: (u16, u16),
    at: Instant,
    pred: Option<Instant>,
}

pub struct Trace {
    file: File,
    keys: Vec<Key>,
}

impl Trace {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("SILO_LATENCY_TRACE")?;
        let file = File::options().create(true).append(true).open(&path);
        let file = file.inspect_err(|error| tracing::warn!("latency trace {}: {error}", path.display())).ok()?;
        Some(Self { file, keys: Vec::new() })
    }

    /// A printable key was sent; `cell` is where it should appear.
    pub fn key(&mut self, text: char, cell: (u16, u16)) {
        self.keys.push(Key { text: text.to_string(), cell, at: Instant::now(), pred: None });
    }

    /// A paint finished at `now` with these predicted cells over `frame`.
    fn painted(&mut self, predicted: &[Predicted], frame: &Frame, now: Instant) {
        let columns = usize::from(frame.size.columns);
        let mut lines = Vec::new();
        self.keys.retain_mut(|key| {
            let (row, column) = key.cell;
            if key.pred.is_none() && predicted.iter().any(|p| (p.row, p.column) == key.cell && p.text == key.text) {
                key.pred = Some(now);
            }
            let index = usize::from(row) * columns + usize::from(column);
            let echo = frame.cells.get(index).is_some_and(|c| c.text == key.text);
            if !echo && now - key.at < GIVE_UP {
                return true;
            }
            let ms = |t: Instant| (t - key.at).as_secs_f64() * 1000.0;
            let line = serde_json::json!({
                "key": key.text,
                "paint_pred_ms": key.pred.map(ms),
                "paint_echo_ms": echo.then(|| ms(now)),
            });
            lines.push(line.to_string());
            false
        });
        for line in lines {
            if let Err(error) = writeln!(self.file, "{line}") {
                tracing::warn!("latency trace: {error}");
            }
        }
    }
}

/// Painted after the terminal canvas: stamps the end of this frame's paint.
pub fn paint_probe(trace: Rc<RefCell<Trace>>, prepared: Arc<PreparedFrame>, predicted: Vec<Predicted>) -> impl IntoElement {
    canvas(|_, _, _| {}, move |_, (), _, _| trace.borrow_mut().painted(&predicted, prepared.frame(), Instant::now()))
        .absolute()
        .size_full()
}
