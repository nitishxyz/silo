//! Line-by-line reveal of streaming assistant text (`docs/AGENT_BLOCK.md` §3):
//! a wrapped line shows once it's complete (a later line or block began, or
//! the stream ended); the message's height glides open for it, and once its
//! slot is mostly open it fades in where it ends up (nothing slides out from
//! under a clip). A burst of lines is paced, never far behind the stream.
//! Pure: the view feeds back each leaf's visual line starts from its last
//! layout.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use super::markdown::Block;
use crate::motion;

/// Lines of a burst follow each other this far apart…
pub const STAGGER: Duration = Duration::from_millis(40);
/// …but never more than this behind the stream.
pub const LAG: Duration = Duration::from_millis(160);
/// A line fades in over this long.
pub const FADE: Duration = motion::UI_FAST;
/// A line's fade starts at least this long after its slot began to open, and
/// only once the height drawn is within [`SLACK`] of it.
pub const OPEN: Duration = Duration::from_millis(60);
/// How far a message may draw past the height it was given: the last couple of
/// points of a slot still opening (within the gap above the activity row).
pub const SLACK: f32 = 4.0;
/// Arrivals are instant this long after a quiet start (opened mid-stream) or a width change.
const QUIET: Duration = Duration::from_millis(150);
/// A text line's height before it was laid out, and a code line's.
const TEXT_LINE: f32 = 21.0;
pub const CODE_LINE: f32 = 18.0;
/// A code box's padding and bottom margin around its lines.
pub const CODE_BOX: f32 = 30.0;

/// A leaf block's lines: wrapped text (a paragraph, which a blank line
/// completes; a heading, which its newline completes) or fixed lines (code,
/// a rule) that a newline completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Para,
    Line,
    Fixed(usize),
}

/// Leaves of `block` in reading order, with the ids the view gives them.
pub fn leaves(id: &str, block: &Block, out: &mut Vec<(String, Kind)>) {
    match block {
        Block::Para(_) => out.push((id.to_owned(), Kind::Para)),
        Block::Heading(..) => out.push((id.to_owned(), Kind::Line)),
        Block::Code { text, .. } => out.push((id.to_owned(), Kind::Fixed(text.lines().count()))),
        Block::Rule => out.push((id.to_owned(), Kind::Fixed(1))),
        Block::List { items, .. } => {
            for (i, blocks) in items.iter().enumerate() {
                for (j, b) in blocks.iter().enumerate() {
                    leaves(&format!("{id}-{i}-{j}"), b, out);
                }
            }
        }
        Block::Quote(blocks) => {
            for (i, b) in blocks.iter().enumerate() {
                leaves(&format!("{id}-q{i}"), b, out);
            }
        }
    }
}

/// Whether the text's last leaf has its last line complete at the end of `source`.
fn closed(source: &str, kind: Kind) -> bool {
    match kind {
        Kind::Para => source.trim_end_matches([' ', '\t']).ends_with("\n\n"),
        Kind::Line | Kind::Fixed(_) => source.ends_with('\n'),
    }
}

/// When the next line arrives: now, or `stagger` after the one before, at most `lag` from now.
pub fn pace(last: Option<Instant>, now: Instant, stagger: Duration, lag: Duration) -> Instant {
    last.map_or(now, |l| (l + stagger).max(now)).min(now + lag)
}

/// A leaf's visual line starts (bytes into its text) and line height, as last laid out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wrap {
    pub starts: Vec<usize>,
    pub line_h: f32,
}

/// What a frame shows of the message.
#[derive(Debug, Default)]
pub struct Shown {
    /// Each leaf not fully shown: its lines' starts (bytes for text, line indices otherwise) and opacity.
    /// A fixed leaf (code) lists only its complete lines: the one being written isn't drawn.
    pub lines: HashMap<String, Vec<(usize, f32)>>,
    /// Height held back at the end (the text line being written, drawn transparent).
    pub held: f32,
    /// Top-level blocks before this are fully shown.
    pub settled: usize,
    /// A line is fading in or waiting for its turn.
    pub moving: bool,
    /// Everything shows and the stream is over.
    pub done: bool,
}

pub struct Reveal {
    /// When each revealed line of a leaf arrived (or will), and the height the message is given once its slot
    /// is open (the target when it completed): it doesn't start fading while less than that, less
    /// [`SLACK`], is drawn.
    arrivals: HashMap<String, Vec<(Instant, f32)>>,
    last: Option<Instant>,
    quiet_until: Option<Instant>,
    settled: usize,
    width: f32,
    pub wraps: Rc<RefCell<HashMap<String, Wrap>>>,
    pub height: motion::Glide,
    pub measured: Rc<Cell<f32>>,
    /// The height drawn at the last frame.
    pub drawn: f32,
    /// Frames drawn since everything showed and the height settled.
    pub idle: u8,
}

impl Reveal {
    /// `quiet`: what's there already shows at once (the message was streaming before it was seen).
    pub fn new(quiet: bool, now: Instant) -> Self {
        Self {
            arrivals: HashMap::new(),
            last: None,
            quiet_until: quiet.then_some(now + QUIET),
            settled: 0,
            width: 0.0,
            wraps: Rc::default(),
            height: motion::Glide::new(motion::UI),
            measured: Rc::default(),
            drawn: 0.0,
            idle: 0,
        }
    }

    /// Lines show at once (and the height follows at once).
    pub fn quiet(&self, now: Instant) -> bool {
        self.quiet_until.is_some_and(|q| now < q)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn frame(&mut self, key: &str, blocks: &[Block], source: &str, streaming: bool, width: f32, now: Instant, reduce: bool) -> Shown {
        // Wider or narrower, the lines wrap anew: nothing fades again.
        if (width - self.width).abs() > 0.5 {
            if self.width > 0.0 {
                self.quiet_until = Some(now + QUIET);
            }
            self.width = width;
        }
        let instant = self.quiet(now) || reduce;
        let slow = motion::slowdown();
        let (stagger, lag, open_at) = (STAGGER.mul_f32(slow), LAG.mul_f32(slow), now + OPEN.mul_f32(slow));
        let shown = now.checked_sub(FADE.mul_f32(slow)).unwrap_or(now);
        self.settled = self.settled.min(blocks.len());
        let base = self.settled;
        let mut leafs = Vec::new();
        let mut ends = Vec::new();
        for (i, b) in blocks.iter().enumerate().skip(base) {
            leaves(&format!("{key}-{i}"), b, &mut leafs);
            ends.push(leafs.len());
        }
        let mut out = Shown { settled: base, ..Shown::default() };
        let mut full = Vec::with_capacity(leafs.len());
        let mut fresh = Vec::new();
        let drawn = self.drawn + SLACK;
        let wraps = self.wraps.borrow();
        for (ix, (id, kind)) in leafs.iter().enumerate() {
            let last = ix + 1 == leafs.len();
            let wrap = wraps.get(id);
            let count = match kind {
                Kind::Fixed(n) => Some(*n),
                Kind::Para | Kind::Line => wrap.map(|w| w.starts.len()),
            };
            // A code block just opened waits for its first line.
            let writing = streaming && last && (!closed(source, *kind) || *kind == Kind::Fixed(0));
            let complete = count.map_or(0, |c| if writing { c.saturating_sub(1) } else { c });
            let arrivals = self.arrivals.entry(id.clone()).or_default();
            let before = arrivals.len();
            while arrivals.len() < complete {
                let at = if instant { shown } else { pace(self.last, open_at, stagger, lag) };
                if !instant {
                    self.last = Some(at);
                    // A code line isn't drawn until it completes, so the last layout doesn't hold it (nor, for
                    // the first, the box's padding).
                    let v = arrivals.len();
                    let extra = match kind {
                        Kind::Fixed(_) => (v + 1 - before) as f32 * CODE_LINE + if before == 0 { CODE_BOX } else { 0.0 },
                        _ => 0.0,
                    };
                    fresh.push((id.clone(), v, extra));
                }
                arrivals.push((at, 0.0));
            }
            // A slot still opening (the height lags a burst) holds its line back.
            for (at, need) in arrivals.iter_mut() {
                if *need > drawn && now > *at {
                    *at = now;
                }
            }
            out.moving |= arrivals.iter().any(|(at, _)| motion::enter(*at, FADE, now, reduce).is_some());
            let alpha =
                |v: usize| arrivals.get(v).map_or(0.0, |(at, _)| if now <= *at { 0.0 } else { motion::enter(*at, FADE, now, reduce).unwrap_or(1.0) });
            let lines: Vec<(usize, f32)> = match (kind, count) {
                (_, None) => vec![(0, 0.0)],
                (Kind::Fixed(_), Some(_)) => (0..complete).map(|v| (v, alpha(v))).collect(),
                (_, Some(c)) => (0..c).map(|v| (wrap.map_or(0, |w| w.starts[v]), alpha(v))).collect(),
            };
            if last {
                out.held = match kind {
                    Kind::Fixed(_) => 0.0,
                    _ => count.map_or(1, |c| c - complete) as f32 * wrap.map_or(TEXT_LINE, |w| w.line_h),
                };
            }
            let shown_all = count.is_some() && !writing && lines.iter().all(|(_, a)| *a >= 1.0);
            if !shown_all {
                out.lines.insert(id.clone(), lines);
            }
            full.push(shown_all);
        }
        drop(wraps);
        // Lines that completed this frame open the height to here.
        let target = (self.measured.get() - out.held).max(0.0);
        for (id, v, extra) in fresh {
            if let Some(a) = self.arrivals.get_mut(&id).and_then(|a| a.get_mut(v)) {
                a.1 = target + extra;
            }
        }
        // Leading blocks fully shown (and not the one still being written) are done with.
        let mut from = 0;
        for (bi, end) in ends.iter().enumerate() {
            let i = base + bi;
            if !full[from..*end].iter().all(|f| *f) || (streaming && i + 1 == blocks.len()) {
                break;
            }
            let mut wraps = self.wraps.borrow_mut();
            for (id, _) in &leafs[from..*end] {
                self.arrivals.remove(id);
                wraps.remove(id);
            }
            out.settled = i + 1;
            from = *end;
        }
        self.settled = out.settled;
        out.done = !streaming && self.settled == blocks.len();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::markdown::parse;

    const MS: Duration = Duration::from_millis(1);

    fn wrap(reveal: &Reveal, id: &str, starts: &[usize]) {
        reveal.wraps.borrow_mut().insert(id.to_owned(), Wrap { starts: starts.to_vec(), line_h: 21.0 });
    }

    #[test]
    fn a_burst_is_paced_and_never_far_behind() {
        let now = Instant::now();
        assert_eq!(pace(None, now, STAGGER, LAG), now);
        let mut last = None;
        let ats: Vec<Instant> = (0..8)
            .map(|_| {
                let at = pace(last, now, STAGGER, LAG);
                last = Some(at);
                at
            })
            .collect();
        assert_eq!(ats[1] - ats[0], STAGGER);
        assert!(ats.iter().all(|at| *at - now <= LAG), "capped");
        assert_eq!(ats[7] - now, LAG);
        // Time passed since: the next one comes at once.
        let later = now + Duration::from_secs(1);
        assert_eq!(pace(last, later, STAGGER, LAG), later);
    }

    #[test]
    fn lines_show_once_complete_and_fade_in() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "One two three four\n\nSecond par";
        let blocks = parse(src);
        // Nothing laid out yet: hidden, a line's height held back.
        let s = r.frame("k", &blocks, src, true, 400.0, now, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0)]);
        assert_eq!(s.lines["k-1"], [(0, 0.0)]);
        assert!((s.held - 21.0).abs() < 1e-3);
        // Laid out: the first paragraph (two visual lines) is complete, the second's only line is being written.
        wrap(&r, "k-0", &[0, 9]);
        wrap(&r, "k-1", &[0]);
        let s = r.frame("k", &blocks, src, true, 400.0, now, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0), (9, 0.0)], "arrived now: still transparent");
        assert_eq!(s.lines["k-1"], [(0, 0.0)], "the line being written is held");
        assert!((s.held - 21.0).abs() < 1e-3 && s.moving);
        // Their slot opens first: both fade in only after `OPEN`, the second a stagger later.
        let s = r.frame("k", &blocks, src, true, 400.0, now + OPEN - MS, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0), (9, 0.0)], "the slot is still opening");
        let mid = now + OPEN + STAGGER + FADE / 2;
        let s = r.frame("k", &blocks, src, true, 400.0, mid, false);
        let (a, b) = (s.lines["k-0"][0].1, s.lines["k-0"][1].1);
        assert!(a > b && b > 0.0 && a < 1.0, "the second line follows the first: {a} {b}");
        let s = r.frame("k", &blocks, src, true, 400.0, now + Duration::from_secs(1), false);
        assert!(!s.lines.contains_key("k-0"), "fully shown");
        assert_eq!(s.settled, 1, "and done with");
        assert!(!s.moving && !s.done);
        // The stream ends: the held line comes promptly.
        let end = now + Duration::from_secs(2);
        let s = r.frame("k", &blocks, src, false, 400.0, end, false);
        assert!(s.held.abs() < 1e-3 && s.moving && !s.done);
        let s = r.frame("k", &blocks, src, false, 400.0, end + OPEN + FADE, false);
        assert!(s.lines.is_empty() && s.done && s.settled == 2);
    }

    #[test]
    fn complete_lines_wait_while_their_slot_is_still_opening() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "One\n\nTwo";
        let blocks = parse(src);
        wrap(&r, "k-0", &[0]);
        wrap(&r, "k-1", &[0]);
        // Laid out 50 tall, the line being written held back: "One" opens the message to 29.
        r.measured.set(50.0);
        r.frame("k", &blocks, src, true, 400.0, now, false);
        // Its time came, but only 10 is drawn (the height lags a burst): it waits.
        r.drawn = 10.0;
        let late = now + OPEN + FADE;
        let s = r.frame("k", &blocks, src, true, 400.0, late, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0)], "its slot is still opening");
        // Nearly open: it fades in from then, where it ends up.
        r.drawn = 29.0 - SLACK / 2.0;
        r.frame("k", &blocks, src, true, 400.0, late, false);
        let s = r.frame("k", &blocks, src, true, 400.0, late + FADE / 2, false);
        let a = s.lines["k-0"][0].1;
        assert!(a > 0.0 && a < 1.0, "{a}");
    }

    #[test]
    fn code_lines_complete_at_their_newline_and_lists_by_item() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "```sh\nls\npwd";
        let blocks = parse(src);
        let s = r.frame("k", &blocks, src, true, 400.0, now, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0)], "`pwd` is still being typed: not drawn, no height held");
        assert!(s.held.abs() < 1e-3);
        let src = "```sh\nls\npwd\n";
        let s = r.frame("k", &parse(src), src, true, 400.0, now, false);
        assert_eq!(s.lines["k-0"].len(), 2);
        let src = "- one\n- tw";
        let blocks = parse(src);
        let mut r = Reveal::new(false, now);
        wrap(&r, "k-0-0-0", &[0]);
        wrap(&r, "k-0-1-0", &[0]);
        let s = r.frame("k", &blocks, src, true, 400.0, now + Duration::from_secs(1), false);
        assert!(s.lines.contains_key("k-0-0-0") && s.lines["k-0-1-0"] == [(0, 0.0)]);
        assert_eq!(s.settled, 0, "the list is still being written");
    }

    #[test]
    fn quiet_and_reduced_motion_show_at_once() {
        let now = Instant::now();
        let src = "Already here.\n\nAnd more";
        let blocks = parse(src);
        for (quiet, reduce) in [(true, false), (false, true)] {
            let mut r = Reveal::new(quiet, now);
            wrap(&r, "k-0", &[0]);
            wrap(&r, "k-1", &[0]);
            let s = r.frame("k", &blocks, src, true, 400.0, now + MS, reduce);
            assert!(!s.lines.contains_key("k-0") && !s.moving, "{quiet} {reduce}");
            assert_eq!(s.lines["k-1"], [(0, 0.0)], "the line being written is still held");
        }
        // A width change: the rewrapped lines don't fade again.
        let mut r = Reveal::new(false, now);
        wrap(&r, "k-0", &[0]);
        wrap(&r, "k-1", &[0]);
        let late = now + Duration::from_secs(1);
        r.frame("k", &blocks, src, true, 400.0, now, false);
        wrap(&r, "k-1", &[0, 4]);
        let s = r.frame("k", &blocks, src, true, 300.0, late, false);
        assert_eq!(s.lines["k-1"], [(0, 1.0), (4, 0.0)]);
    }
}
