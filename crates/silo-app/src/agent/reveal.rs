//! Line-by-line reveal of streaming assistant text (`docs/AGENT_BLOCK.md` §3):
//! a wrapped line shows once it's complete (a later line or block began, or
//! the stream ended); the message's height glides open for it once its turn is
//! near (the lines before it started: space opens line by line with the
//! sweeps, never a burst's worth ahead), and once its slot is mostly open it
//! sweeps in left to right where it ends up (nothing slides out from under a
//! clip): a soft edge moves across it at a constant speed, the next line
//! starting halfway through. A burst is paced, never far behind the stream
//! (behind, sweeps speed up). Pure: the view feeds back each leaf's visual
//! lines (starts, widths, glyph x positions, top) from its last layout.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use super::markdown::Block;
use crate::motion;

/// A full-width line sweeps in over this long (a shorter one sooner: constant speed)…
pub const SWEEP: Duration = Duration::from_millis(250);
/// …but never quicker than this share of it.
const SHORTEST: f32 = 0.2;
/// The sweep's soft edge, as a share of the line's width.
pub const EDGE: f32 = 0.2;
/// A line starts sweeping at most this long after it completed (behind, sweeps speed up).
pub const LAG: Duration = Duration::from_millis(200);
/// Segments a sweeping text line is cut into.
pub const SEGMENTS: usize = 16;
/// A line's sweep starts at least this long after its slot began to open, and
/// only once the height drawn is within [`SLACK`] of it.
pub const OPEN: Duration = Duration::from_millis(60);
/// The message's height glides this long toward its target (a line's slot opens in about a third of it).
pub const GLIDE: Duration = motion::UI_FAST;
/// How far a message may draw past the height it was given: the last couple of
/// points of a slot still opening (within the gap above the activity row).
pub const SLACK: f32 = 4.0;
/// Arrivals are instant this long after a quiet start (opened mid-stream) or a width change.
const QUIET: Duration = Duration::from_millis(150);
/// A text line's height before it was laid out, and a code line's.
const TEXT_LINE: f32 = 21.0;
pub const CODE_LINE: f32 = 18.0;
/// A code box's padding (sideways, above and below), font size and bottom margin.
pub const CODE_PAD_X: f32 = 12.0;
pub const CODE_PAD_Y: f32 = 10.0;
pub const CODE_FONT: f32 = 12.5;
pub const CODE_MARGIN: f32 = 10.0;
/// A code box's padding and bottom margin around its lines.
const CODE_BOX: f32 = 2.0 * CODE_PAD_Y + CODE_MARGIN;

/// A leaf block's lines: wrapped text (a paragraph, which a blank line
/// completes; a heading, which its newline completes) or fixed lines (code,
/// a rule) that a newline completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Para,
    Line,
    Fixed(usize),
}

/// Leaves of `block` in reading order, with the ids the view gives them (and a code block's text).
pub fn leaves<'a>(id: &str, block: &'a Block, out: &mut Vec<(String, Kind, &'a str)>) {
    match block {
        Block::Para(_) => out.push((id.to_owned(), Kind::Para, "")),
        Block::Heading(..) => out.push((id.to_owned(), Kind::Line, "")),
        Block::Code { text, .. } => out.push((id.to_owned(), Kind::Fixed(text.lines().count()), text)),
        Block::Rule => out.push((id.to_owned(), Kind::Fixed(1), "")),
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

/// Columns between a code box's tab stops.
pub const TAB: usize = 4;

/// A code line's width in cells: a tab runs to the next stop, East Asian wide characters and emoji take two.
pub fn display_width(line: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    line.split('\t').enumerate().fold(0, |w, (i, run)| if i == 0 { w } else { (w / TAB + 1) * TAB } + run.width())
}

/// A code line as drawn: tabs expanded to spaces up to the next stop.
pub fn expand_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\t') {
        return line.into();
    }
    use unicode_width::UnicodeWidthStr;
    let (mut out, mut w) = (String::with_capacity(line.len() + TAB), 0);
    for (i, run) in line.split('\t').enumerate() {
        if i > 0 {
            let pad = (w / TAB + 1) * TAB - w;
            out.extend(std::iter::repeat_n(' ', pad));
            w += pad;
        }
        out.push_str(run);
        w += run.width();
    }
    out.into()
}

/// A code line's width as a share of its box's visible width, `cell` being a character's advance as a share
/// of it (monospace; 0: not laid out yet, the full width).
pub fn code_share(line: &str, cell: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let w = display_width(line) as f32 * cell;
    if cell > 0.0 { w.min(1.0) } else { 1.0 }
}

/// How long a line `share` of the full width takes to sweep (constant speed, `slow`: `motion::slowdown`).
pub fn sweep_time(share: f32, slow: f32) -> Duration {
    SWEEP.mul_f32(share.clamp(SHORTEST, 1.0) * slow)
}

/// When the next line starts its sweep (`now`: the earliest, once its slot opened) and how long it takes
/// (`dur` at full speed): halfway through the one before, no more than `lag` from now; a line queued
/// more than half of `lag` behind sweeps faster. Past the cap a burst still cascades, a frame apart.
pub fn pace(prev: Option<(Instant, Duration)>, now: Instant, dur: Duration, lag: Duration) -> (Instant, Duration) {
    let want = prev.map_or(now, |(at, d)| (at + d / 2).max(now));
    let behind = (want - now).as_secs_f32();
    let speed = if behind > 0.0 { (lag.as_secs_f32() / (2.0 * behind)).clamp(0.4, 1.0) } else { 1.0 };
    let at = prev.map_or(want, |(p, _)| want.min(now + lag).max(p + lag / CASCADE));
    (at, if speed < 1.0 { dur.mul_f32(speed) } else { dur })
}

/// Lines past the lag cap start this share of it apart (about a frame).
const CASCADE: u32 = 12;

/// How far a sweep started at `at` and lasting `dur` is at `now` (0 before, 1 once done).
pub fn progress(at: Instant, dur: Duration, now: Instant) -> f32 {
    if now < at {
        0.0
    } else if dur.is_zero() {
        1.0
    } else {
        (now.saturating_duration_since(at).as_secs_f32() / dur.as_secs_f32()).min(1.0)
    }
}

/// Opacity at `u` (0 … 1 across the line) of a sweep `p` (0 … 1) through: a soft edge [`EDGE`] wide
/// moves from before the line's start to past its end.
pub fn sweep(u: f32, p: f32) -> f32 {
    ((p * (1.0 + EDGE) - u) / EDGE).clamp(0.0, 1.0)
}

/// A mask over a sweeping line (its background colour drawn over what isn't shown yet): two gradient stops
/// (at, opacity of the mask) within 0 … 1 that reproduce [`sweep`].
pub fn mask(p: f32) -> [(f32, f32); 2] {
    let (a, b) = ((p * (1.0 + EDGE) - EDGE).clamp(0.0, 1.0), (p * (1.0 + EDGE)).clamp(0.0, 1.0));
    [(a, 1.0 - sweep(a, p)), (b, 1.0 - sweep(b, p))]
}

/// The share of a code box's first line sweep in which the box fades in (its text waits: a mask under the
/// box's own fade would let the whole line through).
const BOX_IN: f32 = EDGE / (1.0 + EDGE);

/// A revealed code box's opacity, its first line `p` through.
pub fn code_box(p: f32) -> f32 {
    (p / BOX_IN).clamp(0.0, 1.0)
}

/// How far code line `v`, `p` through, has swept: the first starts once its box is in.
pub fn code_line(v: usize, p: f32) -> f32 {
    if v == 0 { ((p - BOX_IN) / (1.0 - BOX_IN)).clamp(0.0, 1.0) } else { p }
}

/// Cuts of a visual line into about `n` segments: from its glyphs (byte, x from the line's start) in order,
/// the first and each that starts a new `width / n` stretch, as (byte, share of `width`).
pub fn cuts(glyphs: impl IntoIterator<Item = (usize, f32)>, width: f32, n: usize) -> Vec<(usize, f32)> {
    let mut out = Vec::with_capacity(n + 1);
    let width = width.max(1.0);
    let step = width / n.max(1) as f32;
    let mut next = f32::MIN;
    for (byte, x) in glyphs {
        if x >= next {
            out.push((byte, (x / width).clamp(0.0, 1.0)));
            next = ((x / step).floor() + 1.0) * step - 0.01;
        }
    }
    out
}

/// Opacity from each byte on (until the next) of a text leaf whose `lines` are (start, progress): a line
/// not started is hidden, a finished one shows, and only one sweeping is cut into its segments (`wrap`).
pub fn fades(lines: &[(usize, f32)], wrap: Option<&Wrap>) -> Vec<(usize, f32)> {
    let mut out = Vec::with_capacity(lines.len());
    for (v, &(start, p)) in lines.iter().enumerate() {
        match wrap.and_then(|w| w.cuts.get(v)).filter(|c| p > 0.0 && p < 1.0 && !c.is_empty()) {
            Some(cuts) => {
                out.push((start, sweep(cuts[0].1 / 2.0, p)));
                for (i, (byte, u)) in cuts.iter().enumerate() {
                    let next = cuts.get(i + 1).map_or(1.0, |c| c.1);
                    out.push(((*byte).max(start), sweep((u + next) / 2.0, p)));
                }
            }
            None => out.push((start, if p >= 1.0 { 1.0 } else { sweep(0.5, p) })),
        }
    }
    out
}

/// A leaf's visual lines, as last laid out: where each starts (bytes into its text), its width as a share
/// of the text's, its sweep segments ([`cuts`]); the line height and its top (window coordinates, as the
/// message's [`Reveal::origin`]). A code box reports only `cell`, a character's advance as a share of its
/// visible width, and its top; a rule its top.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wrap {
    pub starts: Vec<usize>,
    pub widths: Vec<f32>,
    pub cuts: Vec<Vec<(usize, f32)>>,
    pub line_h: f32,
    pub cell: f32,
    pub top: Option<f32>,
}

/// What a frame shows of the message.
#[derive(Debug, Default)]
pub struct Shown {
    /// Each leaf not fully shown: its lines' starts (bytes for text, line indices otherwise) and how far
    /// each has swept in (0 hidden … 1 shown).
    /// A fixed leaf (code) lists only its complete lines given their space: the one being written and those
    /// waiting for their turn aren't drawn (the box grows with its revealed lines).
    pub lines: HashMap<String, Vec<(usize, f32)>>,
    /// Height held back at the end (the text line being written, drawn transparent).
    pub held: f32,
    /// The height the message is given: down to the end of the last line that started sweeping or is about
    /// to (at most one line ahead of the text shown), all of it but the line being written once caught up.
    pub height: f32,
    /// Top-level blocks before this are fully shown.
    pub settled: usize,
    /// A line is sweeping in or waiting for its turn.
    pub moving: bool,
    /// Everything shows and the stream is over.
    pub done: bool,
}

pub struct Reveal {
    /// When each revealed line of a leaf starts sweeping (or will), for how long, and the height the message
    /// needs for it when its leaf's top isn't known (the height when it completed): it doesn't start while
    /// less than its bottom, less [`SLACK`], is drawn.
    arrivals: HashMap<String, Vec<(Instant, Duration, f32)>>,
    last: Option<(Instant, Duration)>,
    quiet_until: Option<Instant>,
    settled: usize,
    width: f32,
    pub wraps: Rc<RefCell<HashMap<String, Wrap>>>,
    pub height: motion::Glide,
    pub measured: Rc<Cell<f32>>,
    /// The message's top in window coordinates at the last layout (leaves report theirs alike).
    pub origin: Rc<Cell<f32>>,
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
            height: motion::Glide::new(GLIDE),
            measured: Rc::default(),
            origin: Rc::default(),
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
        // Wider or narrower, the lines wrap anew: nothing sweeps again.
        if (width - self.width).abs() > 0.5 {
            if self.width > 0.0 {
                self.quiet_until = Some(now + QUIET);
            }
            self.width = width;
        }
        let instant = self.quiet(now) || reduce;
        let slow = motion::slowdown();
        let (lag, open_at, origin) = (LAG.mul_f32(slow), now + OPEN.mul_f32(slow), self.origin.get());
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
        // Where the next line held back may start: halfway through the one before (held lines released
        // together keep their order instead of sweeping as a group).
        let mut floor: Option<(Instant, Duration)> = None;
        // Space opens for the lines that started and the next about to (`open`: every line so far started);
        // `gap`: where the first line not given its space yet begins (None inside: its top isn't known);
        // `next`: the end of the line given its space while it waits (the last given: nothing below it opens).
        let (mut open, mut next, mut gap) = (true, None::<Option<f32>>, None::<Option<f32>>);
        for (ix, (id, kind, code)) in leafs.iter().enumerate() {
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
                let v = arrivals.len();
                // Text and code lines sweep across their own width (a rule across the whole).
                let share = match kind {
                    Kind::Fixed(_) if code.is_empty() => 1.0,
                    Kind::Fixed(_) => code_share(code.lines().nth(v).unwrap_or(""), wrap.map_or(0.0, |w| w.cell)),
                    _ => wrap.and_then(|w| w.widths.get(v)).copied().unwrap_or(1.0),
                };
                let (at, dur) = if instant { (now, Duration::ZERO) } else { pace(self.last, open_at, sweep_time(share, slow), lag) };
                if !instant {
                    self.last = Some((at, dur));
                    // A code line isn't drawn until it completes, so the last layout doesn't hold it (nor, for
                    // the first, the box's padding).
                    let extra = match kind {
                        Kind::Fixed(_) => (v + 1 - before) as f32 * CODE_LINE + if before == 0 { CODE_BOX } else { 0.0 },
                        _ => 0.0,
                    };
                    fresh.push((id.clone(), v, extra));
                }
                arrivals.push((at, dur, 0.0));
            }
            let top = wrap.and_then(|w| w.top).map(|t| t - origin);
            let line_h = wrap.map_or(TEXT_LINE, |w| w.line_h);
            #[allow(clippy::cast_precision_loss)]
            let reach = |v: usize| {
                top.map(|t| match kind {
                    Kind::Fixed(_) if code.is_empty() => t + 1.0,
                    Kind::Fixed(_) => t + 2.0 * CODE_PAD_Y + (v + 1) as f32 * CODE_LINE,
                    _ => t + (v + 1) as f32 * line_h,
                })
            };
            let base = |v: usize| if v == 0 { top } else { reach(v - 1) };
            // Lines given their space (a code box draws only these).
            let mut given = 0;
            for (v, (at, dur, need)) in arrivals.iter_mut().enumerate() {
                // A slot still opening (the height lags a burst) holds its line back.
                let held = !instant && reach(v).unwrap_or(*need) > drawn;
                if held {
                    let start = floor.map_or(now, |(a, d)| (a + d / 2).min(now + lag).max(a + lag / CASCADE).max(now));
                    if now > *at || start > *at {
                        *at = start;
                    }
                }
                floor = Some((*at, *dur));
                if open {
                    if *at > open_at {
                        (open, gap) = (false, Some(base(v)));
                    } else {
                        given = v + 1;
                        if held || *at > now {
                            (open, next) = (false, Some(reach(v)));
                        }
                    }
                }
            }
            let swept = |v: usize| arrivals.get(v).map_or(0.0, |(at, dur, _)| progress(*at, *dur, now));
            out.moving |= (0..arrivals.len()).any(|v| swept(v) < 1.0);
            let lines: Vec<(usize, f32)> = match (kind, count) {
                (_, None) => vec![(0, 0.0)],
                (Kind::Fixed(_), Some(_)) => (0..complete.min(given)).map(|v| (v, swept(v))).collect(),
                (_, Some(c)) => (0..c).map(|v| (wrap.map_or(0, |w| w.starts[v]), swept(v))).collect(),
            };
            if last {
                out.held = match kind {
                    Kind::Fixed(_) => 0.0,
                    _ => count.map_or(1, |c| c - complete) as f32 * wrap.map_or(TEXT_LINE, |w| w.line_h),
                };
            }
            let waiting = matches!(kind, Kind::Fixed(_)) && given < complete;
            let shown_all = count.is_some() && !writing && !waiting && lines.iter().all(|(_, a)| *a >= 1.0);
            if !shown_all {
                out.lines.insert(id.clone(), lines);
            }
            full.push(shown_all);
        }
        drop(wraps);
        if let Some(latest) = floor.filter(|(at, _)| self.last.is_none_or(|(last, _)| *at > last)) {
            self.last = Some(latest);
        }
        // Lines that completed this frame need the height to here (when their leaf's top isn't known).
        let target = (self.measured.get() - out.held).max(0.0);
        for (id, v, extra) in fresh {
            if let Some(a) = self.arrivals.get_mut(&id).and_then(|a| a.get_mut(v)) {
                a.2 = target + extra;
            }
        }
        // The last line given its space hasn't started: nothing more below it (the gap and padding after it
        // open once it sweeps).
        out.height = match gap.or(next) {
            Some(Some(g)) => g.max(0.0),
            _ => target,
        };
        // Leading blocks fully shown (and not the one still being written) are done with.
        let mut from = 0;
        for (bi, end) in ends.iter().enumerate() {
            let i = base + bi;
            if !full[from..*end].iter().all(|f| *f) || (streaming && i + 1 == blocks.len()) {
                break;
            }
            let mut wraps = self.wraps.borrow_mut();
            for (id, ..) in &leafs[from..*end] {
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
        let widths = vec![1.0; starts.len()];
        reveal.wraps.borrow_mut().insert(id.to_owned(), Wrap { starts: starts.to_vec(), widths, line_h: 21.0, ..Wrap::default() });
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn the_sweep_is_a_soft_edge_moving_left_to_right() {
        // Before: nothing; after: everything.
        assert!((0..=10).all(|i| sweep(i as f32 / 10.0, 0.0) == 0.0));
        assert!((0..=10).all(|i| sweep(i as f32 / 10.0, 1.0) == 1.0));
        // Midway: the left shows, the right doesn't, a gradient `EDGE` wide between.
        let p = 0.5;
        let front = p * (1.0 + EDGE);
        assert!(close(sweep(0.0, p), 1.0) && close(sweep(front - EDGE, p), 1.0) && close(sweep(front, p), 0.0));
        assert!(close(sweep(front - EDGE / 2.0, p), 0.5), "soft, not a hard wipe");
        // Monotonic: falls to the right, rises with progress.
        for i in 0..10 {
            let (u, v) = (i as f32 / 10.0, (i + 1) as f32 / 10.0);
            assert!(sweep(u, p) >= sweep(v, p));
            assert!(sweep(u, v) >= sweep(u, u));
        }
        // The mask (code boxes) reproduces it between its stops and stays within the box.
        for p in [0.05, 0.3, 0.5, 0.9, 0.98] {
            let [(a, ma), (b, mb)] = mask(p);
            assert!((0.0..=1.0).contains(&a) && a < b && b <= 1.0, "{p}");
            assert!(close(1.0 - ma, sweep(a, p)) && close(1.0 - mb, sweep(b, p)), "{p}");
            let mid = f32::midpoint(a, b);
            assert!(close(1.0 - f32::midpoint(ma, mb), sweep(mid, p)), "linear between: {p}");
        }
    }

    #[test]
    fn sweeps_go_at_a_constant_speed() {
        assert_eq!(sweep_time(1.0, 1.0), SWEEP);
        assert_eq!(sweep_time(0.5, 1.0), SWEEP / 2, "half the width, half the time");
        assert_eq!(sweep_time(0.01, 1.0), SWEEP.mul_f32(SHORTEST), "a word still sweeps");
        assert_eq!(sweep_time(2.0, 1.0), SWEEP);
        assert_eq!(sweep_time(1.0, 12.0), SWEEP * 12, "slowed down");
        let now = Instant::now();
        assert_eq!(progress(now, SWEEP, now - MS), 0.0);
        assert!(close(progress(now, SWEEP, now + SWEEP / 2), 0.5));
        assert_eq!(progress(now, SWEEP, now + SWEEP * 2), 1.0);
        assert_eq!(progress(now, Duration::ZERO, now), 1.0, "at once");
    }

    #[test]
    fn lines_overlap_halfway_and_a_burst_is_never_far_behind() {
        let now = Instant::now();
        assert_eq!(pace(None, now, SWEEP, LAG), (now, SWEEP));
        // The next starts halfway through the one before, at full speed while it's within half the lag.
        let short = SWEEP / 4;
        let (at, dur) = pace(Some((now, short)), now, short, LAG);
        assert_eq!((at, dur), (now + short / 2, short));
        // A burst: capped at `LAG`, and sweeping faster the further behind.
        let mut last = None;
        let paced: Vec<(Instant, Duration)> = (0..10)
            .map(|_| {
                let p = pace(last, now, SWEEP, LAG);
                last = Some(p);
                p
            })
            .collect();
        assert_eq!(paced[1].0 - paced[0].0, SWEEP / 2);
        // Past the cap they cascade a frame apart rather than start together.
        assert!(paced.windows(2).all(|w| w[1].0 - w[0].0 >= LAG / CASCADE), "staggered");
        assert!(paced.iter().enumerate().all(|(i, (at, _))| *at - now <= LAG + LAG / CASCADE * i as u32), "capped");
        assert!(paced[1].1 < SWEEP && paced[9].1 < paced[1].1, "behind: faster");
        assert!(paced.iter().all(|(_, d)| *d >= SWEEP.mul_f32(0.4)), "but still a sweep");
        assert!(paced.iter().all(|(at, d)| *at + *d - now <= LAG * 2 + SWEEP), "all done soon after");
        // Time passed since: the next one comes at once, at full speed.
        let later = now + Duration::from_secs(1);
        assert_eq!(pace(last, later, SWEEP, LAG), (later, SWEEP));
    }

    #[test]
    fn a_sweeping_line_is_cut_into_segments() {
        // 40 glyphs 5 apart (a 200 wide line) in 16 segments: one cut per 12.5 stretch.
        let glyphs: Vec<(usize, f32)> = (0..40).map(|i| (10 + i, i as f32 * 5.0)).collect();
        let c = cuts(glyphs.iter().copied(), 200.0, SEGMENTS);
        assert_eq!(c[0], (10, 0.0));
        assert!((SEGMENTS - 1..=SEGMENTS + 1).contains(&c.len()), "{}", c.len());
        assert!(c.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        assert!(c.windows(2).all(|w| w[1].1 - w[0].1 <= 2.0 * 12.5 / 200.0), "evenly spread");
        // Fewer glyphs than segments: each is its own.
        assert_eq!(cuts([(0, 0.0), (1, 50.0), (5, 120.0)], 200.0, SEGMENTS), [(0, 0.0), (1, 0.25), (5, 0.6)]);
        // Fades: only the sweeping line is segmented; one not started is hidden, one done shows.
        let wrap =
            Wrap { starts: vec![0, 10, 50], widths: vec![1.0; 3], cuts: vec![vec![(0, 0.0)], c, vec![(50, 0.0)]], line_h: 21.0, ..Wrap::default() };
        let f = fades(&[(0, 1.0), (10, 0.5), (50, 0.0)], Some(&wrap));
        assert_eq!(f[0], (0, 1.0));
        assert_eq!(*f.last().unwrap(), (50, 0.0));
        let mid: Vec<&(usize, f32)> = f.iter().filter(|(b, _)| (10..50).contains(b)).collect();
        assert!(mid.len() > SEGMENTS - 2, "segmented");
        assert!(mid.first().unwrap().1 > 0.99 && mid.last().unwrap().1 < 0.01, "shown at the left, not yet at the right");
        assert!(mid.windows(2).all(|w| w[0].1 >= w[1].1 && w[0].0 <= w[1].0));
        // Done: one range per line, nothing segmented.
        assert_eq!(fades(&[(0, 1.0), (10, 1.0)], Some(&wrap)), [(0, 1.0), (10, 1.0)]);
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
        assert_eq!(s.lines["k-0"], [(0, 0.0), (9, 0.0)], "arrived now: not swept yet");
        assert_eq!(s.lines["k-1"], [(0, 0.0)], "the line being written is held");
        assert!((s.held - 21.0).abs() < 1e-3 && s.moving);
        // Their slot opens first: both sweep only after `OPEN`, the second from halfway through the first.
        let s = r.frame("k", &blocks, src, true, 400.0, now + OPEN - MS, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0), (9, 0.0)], "the slot is still opening");
        let s = r.frame("k", &blocks, src, true, 400.0, now + OPEN + SWEEP / 2 - MS, false);
        assert_eq!(s.lines["k-0"][1].1, 0.0, "the second waits for the first's halfway");
        let mid = now + OPEN + SWEEP / 2 + 50 * MS;
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
        let s = r.frame("k", &blocks, src, false, 400.0, end + OPEN + SWEEP, false);
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
        let late = now + OPEN + SWEEP;
        let s = r.frame("k", &blocks, src, true, 400.0, late, false);
        assert_eq!(s.lines["k-0"], [(0, 0.0)], "its slot is still opening");
        // Nearly open: it sweeps in from then, where it ends up.
        r.drawn = 29.0 - SLACK / 2.0;
        r.frame("k", &blocks, src, true, 400.0, late, false);
        let s = r.frame("k", &blocks, src, true, 400.0, late + SWEEP / 2, false);
        assert!(close(s.lines["k-0"][0].1, 0.5));
    }

    #[test]
    fn lines_held_together_are_released_in_order() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "One\n\nTwo\n\nThree";
        let blocks = parse(src);
        for k in ["k-0", "k-1", "k-2"] {
            wrap(&r, k, &[0]);
        }
        r.measured.set(80.0);
        r.frame("k", &blocks, src, true, 400.0, now, false);
        // The height lags well past their turns, then opens at once: they don't all start together.
        let late = now + SWEEP * 4;
        r.frame("k", &blocks, src, true, 400.0, late, false);
        r.drawn = 1000.0;
        let s = r.frame("k", &blocks, src, true, 400.0, late + SWEEP / 4, false);
        let (a, b) = (s.lines["k-0"][0].1, s.lines["k-1"][0].1);
        assert!(a > 0.0 && b == 0.0, "the second waits for the first's halfway: {a} {b}");
        // Still never far behind: held lines aren't chained past the lag cap.
        let s = r.frame("k", &blocks, src, true, 400.0, late + LAG * 2 + SWEEP * 2, false);
        assert!(!s.moving, "{:?}", s.lines);
    }

    #[test]
    fn short_lines_sweep_sooner() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "A long line and a short one\n\nTwo";
        let blocks = parse(src);
        r.wraps.borrow_mut().insert("k-0".into(), Wrap { starts: vec![0, 18], widths: vec![1.0, 0.4], line_h: 21.0, ..Wrap::default() });
        wrap(&r, "k-1", &[0]);
        r.frame("k", &blocks, src, true, 400.0, now, false);
        // The short line starts halfway through the long one and ends well before a full sweep would.
        let s = r.frame("k", &blocks, src, true, 400.0, now + OPEN + SWEEP - 20 * MS, false);
        let p = &s.lines["k-0"];
        assert!(p[0].1 < 1.0 && p[1].1 == 1.0, "{p:?}");
    }

    #[test]
    fn code_lines_sweep_at_a_constant_speed_too() {
        // A character is 2% of the box's visible width: 50 fill it, 25 half, `ls` hardly any.
        assert_eq!(code_share(&"x".repeat(80), 0.02), 1.0, "a long line sweeps the visible part");
        assert!(close(code_share(&"y".repeat(25), 0.02), 0.5));
        assert!(close(code_share("ls", 0.02), 0.04));
        assert_eq!(code_share("ls", 0.0), 1.0, "not laid out yet: the full width");
        // The box fades in before its first line's text sweeps (never a faint whole line under a faint mask).
        assert!(code_box(BOX_IN / 2.0) > 0.0 && code_line(0, BOX_IN) == 0.0 && code_box(BOX_IN) == 1.0);
        assert!(close(code_line(0, 1.0), 1.0) && code_line(1, 0.3) == 0.3);
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        r.wraps.borrow_mut().insert("k-0".into(), Wrap { cell: 0.02, ..Wrap::default() });
        let src = format!("```sh\n{}\nls\n{}\n", "x".repeat(50), "y".repeat(25));
        let blocks = parse(&src);
        r.frame("k", &blocks, &src, true, 400.0, now, false);
        // Their slots are open.
        r.drawn = 1000.0;
        let mut at = |t: Duration| {
            let s = r.frame("k", &blocks, &src, true, 400.0, now + OPEN + t, false);
            s.lines["k-0"].iter().map(|(_, p)| *p).collect::<Vec<f32>>()
        };
        // `ls` starts halfway through the full-width line and takes its floor's time at most (a burst queued
        // past half the lag sweeps a little faster: 40 ms), then the next starts halfway through `ls` (20 ms
        // in), not after a full sweep.
        let p = at(SWEEP / 2 - MS);
        assert_eq!(p[1], 0.0);
        let p = at(SWEEP / 2 + 15 * MS);
        assert!(p[1] > 0.0 && p[2] == 0.0, "not before `ls` is halfway: {p:?}");
        let p = at(SWEEP / 2 + SWEEP.mul_f32(SHORTEST) + MS);
        assert!(p[0] < 1.0 && p[1] == 1.0, "{p:?}");
        assert!(p[2] > 0.0, "the next already started: {p:?}");
    }

    #[test]
    fn code_width_counts_display_cells() {
        assert_eq!(display_width("ls -la"), 6);
        // Tabs run to the next stop of four.
        assert_eq!(display_width("\tx"), TAB + 1);
        assert_eq!(display_width("ab\tc"), TAB + 1);
        assert_eq!(display_width("abcd\te"), 2 * TAB + 1);
        assert_eq!(display_width("a\t\tb"), 2 * TAB + 1);
        // East Asian wide characters and emoji take two cells; combining marks none.
        assert_eq!(display_width("漢字"), 4);
        assert_eq!(display_width("ok 🚀"), 5);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("漢\tx"), TAB + 1, "a tab after a wide character still stops at four");
        assert_eq!(expand_tabs("ab\tc"), "ab  c");
        assert_eq!(expand_tabs("漢\tx"), "漢  x");
        assert_eq!(expand_tabs("\t\tx"), "        x");
        assert!(matches!(expand_tabs("plain"), std::borrow::Cow::Borrowed("plain")));
        // The sweep follows: a wide line sweeps as long as its cells, not its characters.
        assert!(close(code_share("漢字漢字", 0.02), 0.16));
        assert!(close(code_share("\tx", 0.02), 0.1));
    }

    #[test]
    fn a_finished_reply_shows_as_its_last_streaming_frame() {
        let now = Instant::now();
        let mut r = Reveal::new(false, now);
        let src = "One two\n\nThree four";
        let blocks = parse(src);
        wrap(&r, "k-0", &[0]);
        wrap(&r, "k-1", &[0]);
        r.drawn = 1000.0;
        r.frame("k", &blocks, src, true, 400.0, now, false);
        let s = r.frame("k", &blocks, src, true, 400.0, now + Duration::from_secs(1), false);
        assert_eq!(s.lines["k-1"], [(0, 0.0)], "the line being written is held");
        assert_eq!(s.settled, 1);
        // The stream ends with the same text: nothing shown before comes in again; only the held line sweeps.
        let end = now + Duration::from_secs(2);
        let s = r.frame("k", &blocks, src, false, 400.0, end, false);
        assert_eq!((s.settled, s.lines.contains_key("k-0")), (1, false));
        let s = r.frame("k", &blocks, src, false, 400.0, end + OPEN + SWEEP, false);
        assert!(s.done && s.lines.is_empty() && !s.moving);
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
        assert_eq!(s.lines["k-0"].len(), 1, "`pwd` is complete but waits for its turn: the box grows with its lines");
        r.drawn = 1000.0;
        let s = r.frame("k", &parse(src), src, true, 400.0, now + OPEN + SWEEP / 2 + 30 * MS, false);
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

    /// Drives `r` like the view, a frame every 8 ms from `from` to `to`: the height glides to `Shown::height`
    /// and `layout` (given the frame) sets what the last layout reported. Checks every frame with `check`.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        r: &mut Reveal,
        h: &mut motion::Glide,
        src: &str,
        streaming: bool,
        (from, to): (Instant, Instant),
        layout: &dyn Fn(&Reveal, &Shown),
        check: &mut dyn FnMut(&Shown, f32),
    ) -> Shown {
        let blocks = parse(src);
        let mut t = from;
        loop {
            let s = r.frame("k", &blocks, src, streaming, 400.0, t, false);
            check(&s, r.drawn);
            h.target(s.height, t, false);
            r.drawn = h.value(t, false);
            layout(r, &s);
            t += 8 * MS;
            if t > to {
                return s;
            }
        }
    }

    #[test]
    fn a_burst_opens_its_space_line_by_line_with_the_sweeps() {
        // Twelve one-line paragraphs arrive at once (29 apart: a line and its gap), then one being written.
        let now = Instant::now();
        let mut src: String = (0..12).map(|i| format!("Line {i}\n\n")).collect();
        src.push_str("Tail");
        let mut r = Reveal::new(false, now);
        let top = |i: usize| i as f32 * 29.0;
        for i in 0..=12 {
            r.wraps
                .borrow_mut()
                .insert(format!("k-{i}"), Wrap { starts: vec![0], widths: vec![1.0], line_h: 21.0, top: Some(top(i) + 500.0), ..Wrap::default() });
        }
        r.origin.set(500.0);
        r.measured.set(13.0 * 29.0);
        let mut h = motion::Glide::new(GLIDE);
        // The end of the text shown: blocks settled or fully shown, and lines that began to sweep.
        let shown = |s: &Shown| {
            (0..12)
                .filter(|i| *i < s.settled || s.lines.get(&format!("k-{i}")).is_none_or(|l| l[0].1 > 0.0))
                .map(|i| top(i) + 21.0)
                .fold(0.0, f32::max)
        };
        let mut started = Vec::new();
        let s = drive(&mut r, &mut h, &src, true, (now, now + Duration::from_secs(3)), &|_, _| {}, &mut |s, drawn| {
            assert!(s.height <= shown(s) + 29.0 + 0.5, "at most a line (and its gap) ahead of the text: {} {}", s.height, shown(s));
            for i in 0..12 {
                let p = s.lines.get(&format!("k-{i}")).map_or(1.0, |l| l[0].1);
                if p > 0.0 && started.len() == i {
                    assert!(top(i) + 21.0 <= drawn + SLACK, "line {i} sweeps once its slot is open: {drawn}");
                    started.push(i);
                }
            }
        });
        assert_eq!(started.len(), 12, "all shown while the stream goes on");
        assert!((s.height - (13.0 * 29.0 - 21.0)).abs() < 0.5, "caught up: all but the line being written ({})", s.height);
        // Paced: twelve lines in under two seconds (each slot opens as the line before starts).
        let mut r = Reveal::new(false, now);
        for i in 0..=12 {
            r.wraps
                .borrow_mut()
                .insert(format!("k-{i}"), Wrap { starts: vec![0], widths: vec![1.0], line_h: 21.0, top: Some(top(i)), ..Wrap::default() });
        }
        r.measured.set(13.0 * 29.0);
        let mut h = motion::Glide::new(GLIDE);
        let s = drive(&mut r, &mut h, &src, true, (now, now + Duration::from_secs(2)), &|_, _| {}, &mut |_, _| {});
        assert!(s.settled >= 12, "{}", s.settled);
        // The stream ends: the last line comes promptly and the height ends at the whole message.
        let end = now + Duration::from_millis(2500);
        let s = drive(&mut r, &mut h, &src, false, (end, end + Duration::from_secs(1)), &|_, _| {}, &mut |_, _| {});
        assert!(s.done && (s.height - 13.0 * 29.0).abs() < 0.5 && (r.drawn - 13.0 * 29.0).abs() < 0.5, "{} {}", s.height, r.drawn);
    }

    #[test]
    fn a_code_box_grows_with_its_revealed_lines() {
        // Ten code lines arrive at once; the box draws only those given their space (laid out as such).
        let now = Instant::now();
        let mut src = "```sh\n".to_owned();
        for i in 0..10 {
            src.push_str(&format!("echo {i}\n"));
        }
        src.push_str("```\n\nTail");
        let mut r = Reveal::new(false, now);
        r.wraps.borrow_mut().insert("k-0".into(), Wrap { cell: 0.02, top: Some(0.0), ..Wrap::default() });
        let layout = |r: &Reveal, s: &Shown| {
            let n = s.lines.get("k-0").map_or(10, Vec::len) as f32;
            let tail = if n > 0.0 { CODE_BOX + n * CODE_LINE } else { 0.0 };
            r.wraps.borrow_mut().insert("k-1".into(), Wrap { starts: vec![0], widths: vec![1.0], line_h: 21.0, top: Some(tail), ..Wrap::default() });
            r.measured.set(tail + 29.0);
        };
        layout(&r, &Shown::default());
        r.measured.set(29.0);
        let mut h = motion::Glide::new(GLIDE);
        let mut most = 0;
        let s = drive(&mut r, &mut h, &src, true, (now, now + Duration::from_secs(3)), &layout, &mut |s, _| {
            let Some(lines) = s.lines.get("k-0") else { return };
            let sweeping = lines.iter().filter(|(_, p)| *p > 0.0).count();
            assert!(lines.len() <= sweeping + 1, "the box holds the lines shown and one about to: {lines:?}");
            #[allow(clippy::cast_precision_loss)]
            let shown = if sweeping == 0 { 0.0 } else { 2.0 * CODE_PAD_Y + sweeping as f32 * CODE_LINE };
            assert!(s.height <= shown + 2.0 * CODE_PAD_Y + CODE_LINE + 0.5, "{} {shown}", s.height);
            most = most.max(lines.len());
        });
        assert_eq!((most, s.settled), (10, 1), "every line came, the box is done with");
        assert!((s.height - (CODE_BOX + 10.0 * CODE_LINE + 8.0)).abs() < 0.5, "{}", s.height);
    }
}
