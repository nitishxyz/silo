//! Predictive local echo (docs/PHASE5.md §3-4): an overlay of guessed cells
//! and a cursor on top of the mirror, confirmed or rolled back against the
//! mirror after every applied batch. Never touches the mirror itself.

use std::time::{Duration, Instant};

use silo_proto::data::{ServerData, Welcome};
use silo_vt::Frame;

/// Prediction turns on at this smoothed RTT...
pub const RTT_ON: Duration = Duration::from_millis(30);
/// ...and off below this one.
pub const RTT_OFF: Duration = Duration::from_millis(20);

/// The connection's round-trip time with the on/off hysteresis of §4.1.
///
/// It is the transport's own smoothed estimate of the path in use (QUIC's,
/// RFC 9002), not a measurement of input acknowledgements: those include
/// the time a busy UI thread takes to read them, and a connection that
/// switches paths (relay to direct) is followed at once.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rtt {
    srtt: Option<Duration>,
    on: bool,
}

impl Rtt {
    /// The path's current smoothed RTT.
    pub fn update(&mut self, srtt: Duration) {
        self.srtt = Some(srtt);
        if srtt >= RTT_ON {
            self.on = true;
        } else if srtt < RTT_OFF {
            self.on = false;
        }
    }

    #[must_use]
    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// The RTT is high enough to predict.
    #[must_use]
    pub fn on(&self) -> bool {
        self.on
    }
}

/// One predicted cell to paint (viewport coordinates). Empty `text` is a
/// predicted blank (backspace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PredCell {
    pub row: u16,
    pub column: u16,
    pub text: String,
}

#[derive(Clone, Debug)]
struct Pred {
    cell: PredCell,
    /// 1-based `Input` frame count that carried it.
    input: u64,
    epoch: u64,
    sent: Instant,
    /// `next_offset` when it first became judgeable.
    window: Option<u64>,
}

/// Per-pane prediction state; feed it the encoded input, every server
/// frame and the mirror frame after each applied batch.
#[derive(Debug)]
pub struct Predictor {
    remote: bool,
    rtt: Rtt,
    /// `(echo, icanon)` from the last `TtyMode`.
    tty: Option<(bool, bool)>,
    epoch: u64,
    confirmed_epoch: u64,
    cells: Vec<Pred>,
    cursor: Option<(u16, u16)>,
    /// Where this epoch's typing started; backspace stops there.
    start: Option<(u16, u16)>,
    sent: u64,
    acked: (u64, u64),
    acked_at: Instant,
}

impl Predictor {
    /// A local (unix) connection: never predicts.
    #[must_use]
    pub fn local() -> Self {
        Self {
            remote: false,
            rtt: Rtt::default(),
            tty: None,
            epoch: 1,
            confirmed_epoch: 0,
            cells: Vec::new(),
            cursor: None,
            start: None,
            sent: 0,
            acked: (0, 0),
            acked_at: Instant::now(),
        }
    }

    /// A remote connection with its path RTT (`RemoteConn::rtt`).
    #[must_use]
    pub fn remote(rtt: Option<Duration>) -> Self {
        let mut predictor = Self { remote: true, ..Self::local() };
        if let Some(rtt) = rtt {
            predictor.path_rtt(rtt);
        }
        predictor
    }

    /// The connection's current path RTT (`RemoteConn::rtt`); feed it with
    /// every input. Turning prediction off drops what is shown.
    pub fn path_rtt(&mut self, rtt: Duration) {
        let was = self.rtt.on();
        self.rtt.update(rtt);
        if was != self.rtt.on() {
            tracing::debug!(rtt_ms = rtt.as_secs_f64() * 1e3, on = self.rtt.on(), "prediction");
            if !self.rtt.on() {
                self.clear();
            }
        }
    }

    #[must_use]
    pub fn rtt(&self) -> Rtt {
        self.rtt
    }

    /// Prediction may run for this frame (§4.1-4.3).
    #[must_use]
    pub fn enabled(&self, frame: &Frame) -> bool {
        self.remote && self.rtt.on() && !frame.alt_screen && !frame.mouse_tracking && self.tty.is_some_and(|(echo, icanon)| echo || !icanon)
    }

    /// Drop every prediction and start a new (tentative) epoch.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.cursor = None;
        self.start = None;
        self.epoch += 1;
    }

    /// A new data stream (resume or snapshot): input counts restart.
    pub fn begin(&mut self, welcome: &Welcome) {
        self.clear();
        self.sent = 0;
        self.acked = (0, welcome.offset);
    }
}

impl Predictor {
    /// Feed every server frame, in order (before or after `Mirror::apply`).
    pub fn server(&mut self, data: &ServerData) {
        match *data {
            ServerData::InputAck { count, offset } => {
                self.acked = (count, offset);
                self.acked_at = Instant::now();
            }
            ServerData::TtyMode { echo, icanon, .. } => {
                self.tty = Some((echo, icanon));
                if icanon && !echo {
                    self.clear();
                }
            }
            ServerData::SnapshotHead(_) | ServerData::Resized { .. } | ServerData::Configured(_) | ServerData::Exited { .. } => self.clear(),
            ServerData::SnapshotHistory(_)
            | ServerData::SnapshotDone
            | ServerData::Output { .. }
            | ServerData::ClipboardWrite { .. }
            | ServerData::ClipboardRead { .. }
            | ServerData::ClipboardReadUnknown { .. }
            | ServerData::Unknown(_) => {}
        }
    }
}

impl Predictor {
    /// Feed one `Input` frame's encoded bytes right after sending it, with
    /// the mirror's current frame.
    pub fn input(&mut self, bytes: &[u8], frame: &Frame) {
        let now = Instant::now();
        self.sent += 1;
        if !self.enabled(frame) || !frame.scroll.at_bottom() {
            self.clear();
            return;
        }
        let (row, mut column) = self.cursor.unwrap_or((frame.cursor.row, frame.cursor.column));
        for &byte in bytes {
            let text = match byte {
                // Never at or past the last column: no wrap guessing.
                0x20..=0x7e if column + 1 < frame.size.columns => {
                    self.start.get_or_insert((row, column));
                    column += 1;
                    char::from(byte).to_string()
                }
                0x7f | 0x08 if self.start.is_some_and(|start| start.0 == row && start.1 < column) => {
                    column -= 1;
                    String::new()
                }
                _ => return self.boundary(),
            };
            let at = if text.is_empty() { column } else { column - 1 };
            self.cells.retain(|p| (p.cell.row, p.cell.column) != (row, at));
            let cell = PredCell { row, column: at, text };
            self.cells.push(Pred { cell, input: self.sent, epoch: self.epoch, sent: now, window: None });
        }
        self.cursor = Some((row, column));
    }

    /// End the epoch without predicting: later guesses stay hidden until
    /// one is confirmed.
    fn boundary(&mut self) {
        self.epoch += 1;
        self.cursor = None;
        self.start = None;
    }
}

impl Predictor {
    /// Judge predictions against the mirror after each applied batch.
    pub fn judge(&mut self, frame: &Frame, next_offset: u64) {
        if !self.enabled(frame) {
            if !self.cells.is_empty() || self.cursor.is_some() {
                self.clear();
            }
            return;
        }
        let now = Instant::now();
        let srtt = self.rtt.srtt().unwrap_or_default();
        let late = (now - self.acked_at) >= (srtt * 2).max(Duration::from_millis(50));
        let expire = (srtt * 4).max(Duration::from_millis(250));
        let (count, offset) = self.acked;
        let columns = usize::from(frame.size.columns);
        let (mut wrong, mut confirmed) = (false, None);
        self.cells.retain_mut(|p| {
            if count < p.input || next_offset <= offset {
                return now - p.sent < expire;
            }
            let window = *p.window.get_or_insert(next_offset);
            let index = usize::from(p.cell.row) * columns + usize::from(p.cell.column);
            let actual = frame.cells.get(index).map_or("", |c| c.text.as_str());
            let ok = if p.cell.text.is_empty() { actual.trim().is_empty() } else { actual == p.cell.text };
            if ok {
                confirmed = confirmed.max(Some(p.epoch));
                return false;
            }
            wrong |= next_offset > window || late;
            true
        });
        if wrong {
            return self.clear();
        }
        if let Some(epoch) = confirmed {
            self.confirmed_epoch = self.confirmed_epoch.max(epoch);
        }
        if self.cells.is_empty() && count >= self.sent {
            self.cursor = None;
        }
    }

    /// Cells to paint over the mirror and the cursor to paint instead of
    /// the mirror's, while any prediction is shown.
    #[must_use]
    pub fn overlay(&self) -> (Vec<PredCell>, Option<(u16, u16)>) {
        let shown: Vec<PredCell> = self.cells.iter().filter(|p| p.epoch <= self.confirmed_epoch).map(|p| p.cell.clone()).collect();
        let cursor = if shown.is_empty() { None } else { self.cursor };
        (shown, cursor)
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::{data::StartMode, model::TermSize};
    use silo_vt::{GridSize, TerminalCore};

    use super::*;

    struct H {
        vt: TerminalCore,
        p: Predictor,
        offset: u64,
        sent: u64,
    }

    impl H {
        fn new(echo: bool, icanon: bool) -> Self {
            let grid = GridSize { columns: 20, rows: 5, cell_width: 8, cell_height: 16 };
            let mut p = Predictor::remote(Some(Duration::from_secs(1)));
            p.server(&ServerData::TtyMode { offset: 0, echo, icanon });
            Self { vt: TerminalCore::new(grid).unwrap(), p, offset: 0, sent: 0 }
        }
        fn frame(&mut self) -> Frame {
            self.vt.frame().unwrap()
        }
        fn key(&mut self, bytes: &[u8]) {
            let frame = self.frame();
            self.sent += 1;
            self.p.input(bytes, &frame);
        }
        fn ack(&mut self) {
            self.p.server(&ServerData::InputAck { count: self.sent, offset: self.offset });
        }
        fn out(&mut self, bytes: &[u8]) {
            let _ = self.vt.write_output(bytes);
            self.offset += bytes.len() as u64;
            let frame = self.frame();
            self.p.judge(&frame, self.offset);
        }
        fn shown(&self) -> String {
            self.p.overlay().0.iter().map(|c| if c.text.is_empty() { "_" } else { c.text.as_str() }).collect()
        }
        /// Type and echo one char so the epoch is confirmed.
        fn confirm(&mut self, c: u8) {
            self.key(&[c]);
            self.ack();
            self.out(&[c]);
        }
    }

    #[test]
    fn echo_confirms() {
        let mut h = H::new(false, false);
        h.key(b"a");
        assert_eq!(h.shown(), "", "first epoch is tentative");
        h.ack();
        h.out(b"a");
        h.key(b"b");
        assert_eq!(h.p.overlay(), (vec![PredCell { row: 0, column: 1, text: "b".into() }], Some((0, 2))));
        h.ack();
        h.out(b"b");
        assert_eq!(h.p.overlay(), (vec![], None));
    }

    #[test]
    fn no_echo_prompt_never_shows() {
        let mut h = H::new(false, true);
        for c in b"secret" {
            h.key(&[*c]);
            assert_eq!(h.shown(), "");
            h.ack();
            h.out(b"");
        }
        let frame = h.frame();
        assert!(!h.p.enabled(&frame));
    }

    #[test]
    fn wrong_echo_rolls_back_epoch() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"b");
        h.key(b"c");
        assert_eq!(h.shown(), "bc");
        h.ack();
        h.out(b"B");
        h.out(b"C");
        assert_eq!(h.p.overlay(), (vec![], None));
        h.key(b"d");
        assert_eq!(h.shown(), "", "new epoch is tentative");
    }

    #[test]
    fn epoch_after_enter_is_tentative() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"\r");
        h.ack();
        h.out(b"\r\n");
        h.key(b"b");
        assert_eq!(h.shown(), "");
        h.ack();
        h.out(b"b");
        h.key(b"c");
        assert_eq!(h.p.overlay(), (vec![PredCell { row: 1, column: 1, text: "c".into() }], Some((1, 2))));
    }

    #[test]
    fn alt_screen_disables_and_clears() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"b");
        assert_eq!(h.shown(), "b");
        h.out(b"\x1b[?1049h");
        assert_eq!(h.p.overlay(), (vec![], None));
        let frame = h.frame();
        assert!(!h.p.enabled(&frame));
        h.key(b"c");
        assert_eq!(h.shown(), "");
    }

    #[test]
    fn backspace_only_over_predicted_cells() {
        let mut h = H::new(false, false);
        h.out(b"$ ");
        h.confirm(b'a');
        h.key(b"\x7f");
        assert_eq!(h.p.overlay(), (vec![PredCell { row: 0, column: 2, text: String::new() }], Some((0, 2))));
        h.key(b"\x7f");
        assert_eq!(h.p.overlay(), (vec![PredCell { row: 0, column: 2, text: String::new() }], None));
    }

    #[test]
    fn rtt_hysteresis() {
        let ms = Duration::from_millis;
        let mut rtt = Rtt::default();
        rtt.update(ms(40));
        assert!(rtt.on());
        rtt.update(ms(25));
        assert!(rtt.on(), "25 ms stays on");
        rtt.update(ms(10));
        assert!(!rtt.on());
        rtt.update(ms(25));
        assert!(!rtt.on(), "25 ms stays off");
        rtt.update(ms(40));
        assert!(rtt.on());
        assert!(!Predictor::local().enabled(&H::new(true, true).frame()));
    }

    /// A connection that came up over a relay and moved to the LAN stops
    /// predicting with the next key, dropping what it showed.
    #[test]
    fn path_switch_to_a_fast_link_stops_at_once() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"b");
        assert_eq!(h.shown(), "b");
        h.p.path_rtt(Duration::from_millis(2));
        assert_eq!(h.p.overlay(), (vec![], None));
        h.key(b"c");
        assert_eq!(h.shown(), "");
    }

    #[test]
    fn resize_and_welcome_clear() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"b");
        assert_eq!(h.shown(), "b");
        h.p.server(&ServerData::Resized { offset: h.offset, size: TermSize::new(30, 5) });
        assert_eq!(h.p.overlay(), (vec![], None));
        h.key(b"c");
        assert_eq!(h.shown(), "", "tentative after resize");
        h.p.begin(&Welcome { epoch: 2, mode: StartMode::Snapshot, offset: h.offset, size: TermSize::new(20, 5) });
        assert_eq!(h.p.overlay(), (vec![], None));
    }
}
