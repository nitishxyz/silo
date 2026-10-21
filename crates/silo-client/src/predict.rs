//! Predictive local echo (docs/PHASE5.md §3-4): an overlay of guessed cells
//! and a cursor on top of the mirror, confirmed or rolled back against the
//! frame actually painted. Never touches the mirror itself.
//!
//! It also gates which mirror frame is painted (§3.1): a frame from the
//! middle of an output burst that would take back echoed typing, or one a
//! program draws under synchronized output, waits for the burst to settle.

use std::time::{Duration, Instant};

use silo_proto::data::{ServerData, Welcome};
use silo_vt::Frame;

/// Prediction turns on at this smoothed RTT...
pub const RTT_ON: Duration = Duration::from_millis(30);
/// ...and off below this one.
pub const RTT_OFF: Duration = Duration::from_millis(20);

/// An output burst is complete once no output arrived for this long: one
/// keystroke's echo (a line editor's redraws, a suggestion) comes out of
/// the program within a few milliseconds, so a quiet stream holds all of it.
pub const SETTLE: Duration = Duration::from_millis(50);
/// Longest a mid-burst frame waits while output keeps arriving.
pub const HOLD_LIMIT: Duration = Duration::from_millis(150);
/// Longest a synchronized update (mode 2026) holds the painted frame, as in
/// Ghostty.
pub const SYNC_LIMIT: Duration = Duration::from_secs(1);

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

impl PredCell {
    fn shown_in(&self, frame: &Frame) -> bool {
        let index = usize::from(self.row) * usize::from(frame.size.columns) + usize::from(self.column);
        let actual = frame.cells.get(index).map_or("", |c| c.text.as_str());
        if self.text.is_empty() { actual.trim().is_empty() } else { actual == self.text }
    }
}

#[derive(Clone, Debug)]
struct Pred {
    cell: PredCell,
    /// The predicted cursor right after it.
    after: (u16, u16),
    /// 1-based `Input` frame count that carried it.
    input: u64,
    epoch: u64,
    sent: Instant,
    /// Its echo window started and the cell still differs.
    suspect: bool,
}

/// Per-pane prediction state; feed it the encoded input, every server
/// frame, and each new frame before painting it.
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
    /// This epoch's typing the painted frame already shows, and the cursor
    /// after it: a frame that takes either back is mid-burst.
    echoed: Vec<PredCell>,
    echoed_cursor: Option<(u16, u16)>,
    sent: u64,
    acked: (u64, u64),
    /// When output last arrived.
    output_at: Instant,
    /// Since when a mid-burst frame waits, and since when frames come under
    /// synchronized output.
    held_since: Option<Instant>,
    sync_since: Option<Instant>,
    /// A newer frame waits until then at the latest.
    hold_until: Option<Instant>,
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
            echoed: Vec::new(),
            echoed_cursor: None,
            sent: 0,
            acked: (0, 0),
            output_at: Instant::now(),
            held_since: None,
            sync_since: None,
            hold_until: None,
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
        self.boundary();
    }

    /// A new data stream (resume or snapshot): input counts restart.
    pub fn begin(&mut self, welcome: &Welcome) {
        self.clear();
        self.sent = 0;
        self.acked = (0, welcome.offset);
    }

    /// How long a prediction may wait for its echo.
    fn expire(&self) -> Duration {
        (self.rtt.srtt().unwrap_or_default() * 4).max(Duration::from_millis(250))
    }
}

impl Predictor {
    /// Feed every server frame, in order (before or after `Mirror::apply`).
    pub fn server(&mut self, data: &ServerData) {
        self.server_at(data, Instant::now());
    }

    fn server_at(&mut self, data: &ServerData, now: Instant) {
        match data {
            ServerData::InputAck { count, offset } => self.acked = (*count, *offset),
            ServerData::Output { bytes, .. } if !bytes.is_empty() => self.output_at = now,
            ServerData::TtyMode { echo, icanon, .. } => {
                self.tty = Some((*echo, *icanon));
                if *icanon && !*echo {
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
            | ServerData::CommandEnd(_)
            | ServerData::Commands(_)
            | ServerData::Unknown(_) => {}
        }
    }
}

impl Predictor {
    /// Feed one `Input` frame's encoded bytes right after sending it, with
    /// the mirror's current frame.
    pub fn input(&mut self, bytes: &[u8], frame: &Frame) {
        self.input_at(bytes, frame, Instant::now());
    }

    fn input_at(&mut self, bytes: &[u8], frame: &Frame, now: Instant) {
        self.sent += 1;
        if !self.enabled(frame) || !frame.scroll.at_bottom() {
            self.clear();
            return;
        }
        // A mid-burst frame's cursor may be anywhere on the line.
        let echoed = self.echoed_cursor.filter(|_| self.takes_back(frame));
        let (row, mut column) = self.cursor.or(echoed).unwrap_or((frame.cursor.row, frame.cursor.column));
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
            self.cells.push(Pred { cell, after: (row, column), input: self.sent, epoch: self.epoch, sent: now, suspect: false });
        }
        self.cursor = Some((row, column));
    }

    /// End the epoch without predicting: later guesses stay hidden until
    /// one is confirmed, and the typing so far is no longer guarded.
    fn boundary(&mut self) {
        self.epoch += 1;
        self.cursor = None;
        self.start = None;
        self.echoed.clear();
        self.echoed_cursor = None;
    }
}

impl Predictor {
    /// Whether to paint `frame` (the mirror's newest) now instead of the
    /// frame painted. A frame drawn under synchronized output, or one from
    /// the middle of an output burst that would show less of the typing
    /// than the painted one (a line editor clearing or redrawing the line,
    /// the cursor back over echoed text), waits until the burst settles,
    /// a later frame no longer does that, or its limit passes. Call again
    /// with the newest frame at [`deadline`](Self::deadline).
    pub fn present(&mut self, frame: &Frame) -> bool {
        self.present_at(frame, Instant::now())
    }

    fn present_at(&mut self, frame: &Frame, now: Instant) -> bool {
        if frame.synchronized {
            let until = *self.sync_since.get_or_insert(now) + SYNC_LIMIT;
            if now < until {
                self.hold_until = Some(until);
                return false;
            }
        } else {
            self.sync_since = None;
        }
        if self.takes_back(frame) {
            let until = (self.output_at + SETTLE).min(*self.held_since.get_or_insert(now) + HOLD_LIMIT);
            if now < until {
                self.hold_until = Some(until);
                return false;
            }
            // Settled like this: the program rewrote the typing itself.
            self.echoed.clear();
            self.echoed_cursor = None;
        }
        self.held_since = None;
        self.hold_until = None;
        true
    }

    /// Painting `frame` would take back echoed typing.
    fn takes_back(&self, frame: &Frame) -> bool {
        if !self.enabled(frame) {
            return false;
        }
        if self.echoed.iter().any(|cell| !cell.shown_in(frame)) {
            return true;
        }
        // A shown prediction puts the cursor after it anyway.
        let predicting = self.cells.iter().any(|p| p.epoch <= self.confirmed_epoch);
        !predicting && self.echoed_cursor.is_some_and(|cursor| cursor != (frame.cursor.row, frame.cursor.column))
    }

    /// Judge predictions against the frame painted (`next_offset`: the
    /// mirror offset it shows) whenever it changes or at
    /// [`deadline`](Self::deadline).
    ///
    /// A prediction whose echo window started (`INPUT_ACK` received and
    /// output after it painted) is confirmed once the cell shows its text.
    /// While it differs it is only suspect: the echo of earlier keys may
    /// still be arriving in pieces. It is wrong, and the whole epoch rolls
    /// back, once the burst settled with it still differing.
    pub fn judge(&mut self, frame: &Frame, next_offset: u64) {
        self.judge_at(frame, next_offset, Instant::now());
    }

    fn judge_at(&mut self, frame: &Frame, next_offset: u64, now: Instant) {
        if !self.enabled(frame) {
            if !self.cells.is_empty() || self.cursor.is_some() {
                self.clear();
            }
            return;
        }
        let expire = self.expire();
        // Not against a frame older than one held back.
        let settled = now - self.output_at >= SETTLE && self.hold_until.is_none();
        let (count, offset) = self.acked;
        let (mut wrong, mut confirmed) = (false, None);
        let mut echoed = Vec::new();
        self.cells.retain_mut(|p| {
            if count < p.input || next_offset <= offset {
                return now - p.sent < expire;
            }
            if p.cell.shown_in(frame) {
                confirmed = confirmed.max(Some(p.epoch));
                echoed.push((p.cell.clone(), p.after));
                return false;
            }
            p.suspect = true;
            wrong |= settled || now - p.sent >= expire;
            true
        });
        if wrong {
            tracing::debug!(cells = self.cells.len(), "prediction rolled back");
            return self.clear();
        }
        for (cell, after) in echoed {
            if cell.text.is_empty() {
                self.echoed.retain(|c| c.row != cell.row || c.column < cell.column);
            } else {
                self.echoed.retain(|c| (c.row, c.column) != (cell.row, cell.column));
                self.echoed.push(cell);
            }
            self.echoed_cursor = Some(after);
        }
        if let Some(epoch) = confirmed {
            self.confirmed_epoch = self.confirmed_epoch.max(epoch);
        }
        if self.cells.is_empty() && count >= self.sent {
            self.cursor = None;
        }
    }

    /// When to call [`present`](Self::present) and [`judge`](Self::judge)
    /// again if no frame arrives before: a prediction expires, a suspect
    /// one or a held frame settles, or a hold reaches its limit.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let expire = self.expire();
        let settle = self.cells.iter().any(|p| p.suspect).then_some(self.output_at + SETTLE);
        self.cells.iter().map(|p| p.sent + expire).chain(settle).chain(self.hold_until).min()
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

    const MS: Duration = Duration::from_millis(1);

    /// A mirror, a predictor and the frame painted, on a synthetic clock.
    struct H {
        vt: TerminalCore,
        p: Predictor,
        now: Instant,
        offset: u64,
        sent: u64,
        painted: Frame,
        painted_offset: u64,
    }

    impl H {
        fn new(echo: bool, icanon: bool) -> Self {
            let grid = GridSize { columns: 40, rows: 5, cell_width: 8, cell_height: 16 };
            let mut p = Predictor::remote(Some(Duration::from_millis(150)));
            p.server(&ServerData::TtyMode { offset: 0, echo, icanon });
            let mut vt = TerminalCore::new(grid).unwrap();
            let painted = vt.frame().unwrap();
            Self { vt, p, now: Instant::now(), offset: 0, sent: 0, painted, painted_offset: 0 }
        }
        fn frame(&mut self) -> Frame {
            self.vt.frame().unwrap()
        }
        fn key(&mut self, bytes: &[u8]) {
            let frame = self.frame();
            self.sent += 1;
            self.p.input_at(bytes, &frame, self.now);
        }
        /// The server wrote every input sent so far.
        fn ack(&mut self) {
            self.ack_to(self.sent);
        }
        fn ack_to(&mut self, count: u64) {
            self.now += MS;
            self.p.server_at(&ServerData::InputAck { count, offset: self.offset }, self.now);
            self.paint();
        }
        fn out(&mut self, bytes: &[u8]) {
            self.now += MS;
            self.p.server_at(&ServerData::Output { offset: self.offset, bytes: bytes.to_vec() }, self.now);
            let _ = self.vt.write_output(bytes);
            self.offset += bytes.len() as u64;
            self.paint();
        }
        /// Let time pass with no frame, as the app's deadline timer does.
        fn wait(&mut self, by: Duration) {
            self.now += by;
            self.paint();
        }
        fn paint(&mut self) {
            let frame = self.frame();
            if self.p.present_at(&frame, self.now) {
                self.painted = frame;
                self.painted_offset = self.offset;
            }
            let painted = self.painted.clone();
            self.p.judge_at(&painted, self.painted_offset, self.now);
        }
        fn shown(&self) -> String {
            self.p.overlay().0.iter().map(|c| if c.text.is_empty() { "_" } else { c.text.as_str() }).collect()
        }
        /// What the user sees on `row` from `from`: the painted grid with
        /// the overlay on top, and the cursor painted.
        fn seen(&self, row: u16, from: u16) -> (String, u16) {
            let (cells, cursor) = self.p.overlay();
            let columns = self.painted.size.columns;
            let text: String = (from..columns)
                .map(|column| match cells.iter().find(|c| (c.row, c.column) == (row, column)) {
                    Some(c) => {
                        if c.text.is_empty() {
                            " ".to_owned()
                        } else {
                            c.text.clone()
                        }
                    }
                    None => {
                        let cell = &self.painted.cells[usize::from(row) * usize::from(columns) + usize::from(column)];
                        if cell.text.is_empty() { " ".to_owned() } else { cell.text.clone() }
                    }
                })
                .collect();
            let cursor = cursor.unwrap_or((self.painted.cursor.row, self.painted.cursor.column)).1;
            (text.trim_end().to_owned(), cursor)
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
    fn wrong_echo_rolls_back_epoch_once_settled() {
        let mut h = H::new(false, false);
        h.confirm(b'a');
        h.key(b"b");
        h.key(b"c");
        assert_eq!(h.shown(), "bc");
        h.ack();
        h.out(b"B");
        h.out(b"C");
        assert_eq!(h.shown(), "bc", "suspect until the burst settles");
        let deadline = h.p.deadline().unwrap();
        h.wait(deadline - h.now);
        assert_eq!(h.p.overlay(), (vec![], None));
        h.key(b"d");
        assert_eq!(h.shown(), "", "new epoch is tentative");
    }

    /// The echo of earlier keys still arriving in pieces after a later
    /// key's `INPUT_ACK` is no misprediction.
    #[test]
    fn split_echo_keeps_predictions() {
        let mut h = H::new(false, false);
        h.confirm(b'x');
        for k in [b"a", b"b", b"c"] {
            h.key(k);
        }
        h.ack();
        h.out(b"a");
        assert_eq!(h.shown(), "bc");
        h.out(b"b");
        assert_eq!(h.shown(), "c");
        h.wait(SETTLE / 2);
        h.out(b"c");
        assert_eq!(h.p.overlay(), (vec![], None));
        assert_eq!(h.seen(0, 0), ("xabc".into(), 4));
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

    /// A synchronized update split across frames is painted whole.
    #[test]
    fn synchronized_output_is_painted_whole() {
        let mut h = H::new(false, false);
        h.out(b"old");
        h.out(b"\x1b[?2026h\r\x1b[K");
        h.out(b"ne");
        assert_eq!(h.seen(0, 0).0, "old", "mid-update frames wait");
        h.out(b"w\x1b[?2026l");
        assert_eq!(h.seen(0, 0), ("new".into(), 3));
        h.out(b"\x1b[?2026hstuck");
        h.wait(SYNC_LIMIT);
        assert_eq!(h.seen(0, 0).0, "newstuck", "a hold that never ends is bounded");
    }

    /// One server-side event of a recording: the program read input (the
    /// server writes it and acknowledges), or a PTY read of its output.
    enum E {
        In(&'static [u8]),
        Out(&'static [u8]),
    }
    use E::{In, Out};

    /// Replay `recording` (after `prompt`) typed far ahead of the echo: the
    /// first key confirms the epoch, every other one is typed before the
    /// server sees any of them. No painted state may show less of the
    /// typing than the one before or put the cursor further left, and the
    /// end is exactly `typed`.
    fn replay(prompt: &[u8], recording: &[E], typed: &str) {
        let mut h = H::new(false, false);
        h.out(prompt);
        let start = h.frame().cursor.column;
        let keys: Vec<&[u8]> = recording.iter().filter_map(|e| if let In(k) = e { Some(*k) } else { None }).collect();
        h.key(keys[0]);
        let mut read = 0;
        let (mut shown, mut cursor) = (0, start);
        let mut states = Vec::new();
        for (i, event) in recording.iter().enumerate() {
            match event {
                In(_) => {
                    read += 1;
                    h.ack_to(read);
                }
                Out(bytes) => h.out(bytes),
            }
            if h.sent == 1 && h.p.confirmed_epoch == h.p.epoch {
                keys[1..].iter().for_each(|k| h.key(k));
            }
            if h.sent == 1 {
                // Nothing typed is shown yet (the epoch is tentative).
                continue;
            }
            let (line, at) = h.seen(0, start);
            let now = line.chars().zip(typed.chars()).take_while(|(a, b)| a == b).count();
            states.push(format!("{i}: |{line}| cursor {}", i32::from(at) - i32::from(start)));
            assert!(now >= shown && at >= cursor, "frame {i} takes typing back:\n{}", states.join("\n"));
            (shown, cursor) = (now, at);
        }
        h.wait(SETTLE);
        assert_eq!(h.p.overlay(), (vec![], None), "{}", states.join("\n"));
        let end = start + u16::try_from(typed.len()).unwrap();
        assert_eq!(h.seen(0, start), (typed.to_owned(), end));
        assert_eq!((h.painted.cursor.row, h.painted.cursor.column), (0, end), "the grid itself, not the overlay");
    }

    /// bash (readline), keys 3 ms apart: one echo per key.
    #[test]
    fn replay_bash() {
        let rec = [
            In(b"e"),
            Out(b"e"),
            In(b"c"),
            Out(b"c"),
            In(b"h"),
            Out(b"h"),
            In(b"o"),
            Out(b"o"),
            In(b" "),
            Out(b" "),
            In(b"h"),
            Out(b"h"),
            In(b"x"),
            Out(b"x"),
        ];
        replay(b"$ ", &rec, "echo hx");
    }

    /// bash with keys that reached the PTY together: one echo for several.
    #[test]
    fn replay_bash_burst() {
        replay(b"$ ", &[In(b"e"), Out(b"e"), In(b"c"), In(b"h"), In(b"o"), Out(b"cho")], "echo");
    }

    /// zsh with zsh-syntax-highlighting and zsh-autosuggestions, keys 3 ms
    /// apart (as they reach the server bunched by the network): each key's
    /// echo is several redraws that back up over the line, interleaved with
    /// later keys. Recorded from zsh 5.9.
    #[test]
    fn replay_zsh_syntax_highlighting() {
        let rec = [
            In(b"e"),
            Out(b"e"),
            In(b"c"),
            In(b"h"),
            In(b"o"),
            In(b" "),
            Out(b"\x08\x1b[1m\x1b[31me\x1b[0m\x1b[39m"),
            Out(b"\x08\x1b[1m\x1b[31me\x1b[1m\x1b[31mc\x1b[0m\x1b[39m"),
            Out(b"\x08\x08\x1b[0m\x1b[39me\x1b[0m\x1b[39mc"),
            Out(b"h"),
            Out(b"o"),
            In(b"h"),
            Out(b" "),
            Out(b"h"),
            In(b"x"),
            Out(b"\x08\x08\x08\x08\x08\x08\x1b[32me\x1b[32mc\x1b[32mh\x1b[32mo\x1b[39m \x1b[4mh\x1b[24m"),
            Out(b"\x08\x1b[4mh\x1b[4mx\x1b[24m"),
            Out(b"\x08\x08\x1b[24mh\x1b[24mx"),
        ];
        replay(b"% ", &rec, "echo hx");
    }

    /// The same, keys 80 ms apart: an autosuggestion drawn after the cursor
    /// and the cursor moved back over it, then re-highlighting redraws.
    #[test]
    fn replay_zsh_autosuggestions() {
        let rec = [
            In(b"e"),
            Out(b"e"),
            Out(b"\x08\x1b[1m\x1b[31me\x1b[0m\x1b[39m"),
            Out(b"\x08\x1b[1m\x1b[31me\x1b[0m\x1b[39m\x1b[90mcho hi there\x1b[39m\x1b[12D"),
            In(b"c"),
            Out(b"\x08\x1b[1m\x1b[31me\x1b[1m\x1b[31mc\x1b[0m\x1b[39m"),
            In(b"h"),
            Out(b"\x08\x08\x1b[1m\x1b[31me\x1b[1m\x1b[31mc\x1b[1m\x1b[31mh\x1b[0m\x1b[39m"),
            In(b"o"),
            Out(b"\x08\x1b[1m\x1b[31mh\x1b[1m\x1b[31mo\x1b[0m\x1b[39m"),
            Out(b"\x08\x08\x08\x08\x1b[0m\x1b[32me\x1b[0m\x1b[32mc\x1b[0m\x1b[32mh\x1b[0m\x1b[32mo\x1b[39m"),
            In(b" "),
            Out(b"\x08\x1b[32mo\x1b[32m \x1b[39m"),
            Out(b"\x08\x08\x1b[32mo\x1b[39m\x1b[39m "),
            In(b"h"),
            Out(b"\x1b[39mh"),
            Out(b"\x08\x1b[4mh\x1b[24m"),
            In(b"x"),
            Out(b"\x08\x1b[4mh\x1b[39m\x1b[4mx\x1b[24m\x1b[39m \x1b[39m \x1b[39m \x1b[39m \x1b[39m \x1b[39m \x08\x08\x08\x08\x08\x08"),
            Out(b"\x08\x08\x1b[24mh\x1b[24mx"),
        ];
        replay(b"% ", &rec, "echo hx");
    }

    /// fish-style full-line repaint on every key (carriage return, clear,
    /// prompt and the coloured line), split across two reads.
    #[test]
    fn replay_full_line_repaint() {
        let mut rec = Vec::new();
        for line in ["e", "ec", "ech", "echo"] {
            rec.push(In(&line.as_bytes()[line.len() - 1..]));
            rec.push(Out(b"\r\x1b[K"));
            rec.push(Out(Box::leak(format!("> \x1b[31m{line}\x1b[0m").into_bytes().into_boxed_slice())));
        }
        replay(b"> ", &rec, "echo");
    }
}
