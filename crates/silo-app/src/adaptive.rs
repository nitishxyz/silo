//! Adaptive chrome: the background a block's program paints, so the card
//! padding extends it and the window chrome follows the focused block.

use std::time::{Duration, Instant};

use silo_vt::Frame;

use crate::theme;

/// Share of the sampled weight a colour needs to become the backdrop...
pub const ADOPT_SHARE: f32 = 0.6;
/// ...and to stay it.
pub const KEEP_SHARE: f32 = 0.5;
/// A block's backdrop is re-sampled at most this often.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Duration of a chrome colour change.
pub const BLEND: Duration = Duration::from_millis(200);
/// With several panes visible, the chrome goes this far from the theme
/// background toward the focused block's backdrop.
pub const SHARED_MIX: f32 = 0.65;

/// Sampled cells as `(index, weight)`: the left and right columns, which
/// meet the card's side padding, weigh double; the top and bottom rows
/// (often a TUI's bars) and a sparse interior grid weigh one.
fn samples(columns: usize, rows: usize) -> impl Iterator<Item = (usize, u32)> {
    let (sx, sy) = ((columns / 32).max(1), (rows / 16).max(1));
    let last = columns.saturating_sub(1);
    (0..rows).flat_map(move |y| {
        let (start, step) = if y == 0 || y + 1 == rows {
            (1, 1)
        } else if y % sy == 0 {
            (sx, sx)
        } else {
            (last, 1)
        };
        let sides = std::iter::once(0).chain((last > 0).then_some(last)).map(move |x| (y * columns + x, 2));
        sides.chain((start..last).step_by(step).map(move |x| (y * columns + x, 1)))
    })
}

/// The grid's backdrop: `keep` while it holds [`KEEP_SHARE`] of the sampled
/// weight, else a colour holding [`ADOPT_SHARE`]. `counts` is scratch space
/// reused between calls.
pub fn dominant(columns: usize, rows: usize, keep: Option<u32>, counts: &mut Vec<(u32, u32)>, background: impl Fn(usize) -> u32) -> Option<u32> {
    if columns == 0 || rows == 0 {
        return None;
    }
    let total: u32 = samples(columns, rows).map(|(_, w)| w).sum();
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let need = |share: f32| (total as f32 * share).ceil() as u32;
    let (adopt, hold) = (need(ADOPT_SHARE), need(KEEP_SHARE));
    counts.clear();
    let mut seen = 0;
    let weight_of = |counts: &[(u32, u32)], color: u32| counts.iter().find(|(c, _)| *c == color).map_or(0, |&(_, n)| n);
    for (i, weight) in samples(columns, rows) {
        let color = background(i);
        seen += weight;
        let n = match counts.iter_mut().find(|(c, _)| *c == color) {
            Some((_, n)) => {
                *n += weight;
                *n
            }
            None => {
                counts.push((color, weight));
                weight
            }
        };
        if Some(color) == keep && n >= hold {
            return keep;
        }
        // Another colour wins early only once `keep` can no longer hold.
        if n >= adopt && keep.is_none_or(|k| weight_of(counts, k) + (total - seen) < hold) {
            return Some(color);
        }
    }
    counts.iter().find(|&&(_, n)| n >= adopt).map(|&(c, _)| c)
}

/// A terminal's backdrop: its default background when a program changed it
/// (OSC 11), else on the alternate screen the dominant cell background.
/// `None` when it is the configured `default`.
pub fn terminal_backdrop(frame: &Frame, default: u32, keep: Option<u32>, counts: &mut Vec<(u32, u32)>) -> Option<u32> {
    if frame.background != default {
        return Some(frame.background);
    }
    if !frame.alt_screen {
        return None;
    }
    let columns = usize::from(frame.size.columns);
    let rows = usize::from(frame.size.rows).min(frame.cells.len() / columns.max(1));
    dominant(columns, rows, keep, counts, |i| frame.cells[i].background).filter(|&c| c != default)
}

/// A sampled value adopted only once two samples in a row agree.
#[derive(Debug, Default)]
pub struct Stable {
    value: Option<u32>,
    pending: Option<Option<u32>>,
}

impl Stable {
    #[must_use]
    pub fn value(&self) -> Option<u32> {
        self.value
    }

    /// Feed a sample; true when the adopted value changed. While a change
    /// is pending, [`Self::pending`] is true and another sample is due.
    pub fn feed(&mut self, sample: Option<u32>) -> bool {
        if sample == self.value {
            self.pending = None;
            return false;
        }
        if self.pending == Some(sample) {
            self.pending = None;
            self.value = sample;
            return true;
        }
        self.pending = Some(sample);
        false
    }

    #[must_use]
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// The colours of one surface (the window chrome, or a card) for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chrome {
    pub surface: u32,
    /// Opacity of the surface: below 1 only over a blurred window.
    pub alpha: f32,
    pub ink: u32,
    /// Secondary text.
    pub muted: u32,
    /// Secondary icons.
    pub icon: u32,
    /// Titles of inactive tabs.
    pub secondary: u32,
    pub accent: u32,
}

impl Chrome {
    /// An opaque surface.
    #[must_use]
    pub fn on(surface: u32) -> Self {
        let ink = theme::ink_on(surface);
        Self {
            surface,
            alpha: 1.0,
            ink,
            muted: theme::muted_on(surface),
            icon: theme::icon_muted_on(surface),
            secondary: theme::toward_ink(surface, ink, 0.7, theme::TEXT_CONTRAST),
            accent: theme::accent_on(surface),
        }
    }

    /// A veil of `surface` at `alpha` over the blur (the frame's one veil
    /// alpha, from [`crate::material::readable_alpha`]).
    #[must_use]
    pub fn veiled(surface: u32, alpha: f32) -> Self {
        type Key = (u32, u32, u32, u32);
        thread_local! {
            static CACHE: std::cell::RefCell<std::collections::HashMap<Key, Chrome>> = Default::default();
        }
        let (theme_bg, accent) = (theme::background(), theme::accent());
        let key = (surface, alpha.to_bits(), theme_bg, accent);
        CACHE.with_borrow_mut(|cache| {
            if let Some(c) = cache.get(&key) {
                return *c;
            }
            // Blends pass through many colours; keep the cache small.
            if cache.len() >= 256 {
                cache.clear();
            }
            *cache.entry(key).or_insert_with(|| Self::veiled_over(surface, alpha, theme_bg, accent))
        })
    }

    fn veiled_over(surface: u32, alpha: f32, theme_bg: u32, accent: u32) -> Self {
        if alpha >= 1.0 {
            return Self::on(surface);
        }
        let seen = crate::material::seen(surface, alpha, theme_bg);
        let ink = theme::ink_on_all(&seen);
        let toward = |from: f32, min: f32| theme::reading(seen[0], ink, &seen, from, min);
        let accent = if seen.iter().all(|&b| theme::contrast(accent, b) >= theme::ICON_CONTRAST) { accent } else { ink };
        Self {
            surface,
            alpha,
            ink,
            muted: toward(0.55, theme::TEXT_CONTRAST),
            icon: toward(0.55, theme::ICON_CONTRAST),
            secondary: toward(0.7, theme::TEXT_CONTRAST),
            accent,
        }
    }

    /// These colours for a surface over the app's own content (a float,
    /// a zooming or closing pane): the window blur cannot blur what the
    /// app drew beneath it, so a veil becomes its look as one opaque colour.
    #[must_use]
    pub fn floating(self) -> Self {
        let c = if self.alpha < 1.0 { Self::on(crate::material::look(self.surface, self.alpha)) } else { self };
        debug_assert!(c.alpha >= 1.0, "a floating surface is opaque");
        c
    }

    /// Partway, by `t` from 0 to 1, from this veil to [`Self::floating`],
    /// blended as both would look over a typical desktop.
    #[must_use]
    pub fn toward_floating(self, t: f32) -> Self {
        if self.alpha >= 1.0 || t <= 0.0 {
            return self;
        }
        if t >= 1.0 {
            return self.floating();
        }
        let alpha = self.alpha + (1.0 - self.alpha) * t;
        let look = crate::material::look(self.surface, self.alpha);
        let channel = |shift: u32| {
            let c = |v: u32| ((v >> shift) & 0xff) as f32;
            let v = ((1.0 - t) * self.alpha * c(self.surface) + t * c(look)) / alpha;
            (v.round().clamp(0.0, 255.0) as u32) << shift
        };
        Self::veiled(channel(16) | channel(8) | channel(0), alpha)
    }

    /// The surface's own paint.
    #[must_use]
    pub fn fill(&self) -> gpui::Rgba {
        gpui::rgb(self.surface).opacity(self.alpha)
    }

    /// Ink laid over the surface: a solid step on a solid surface, a tint
    /// on a veil (never an opaque slab over the blur).
    #[must_use]
    pub fn tint(&self, amount: f32) -> gpui::Rgba {
        gpui::rgb(self.ink).opacity(amount)
    }

    /// The current tab, and a hovered control.
    #[must_use]
    pub fn selected(&self) -> gpui::Rgba {
        self.tint(if theme::light(self.surface) { 0.08 } else { 0.10 })
    }

    #[must_use]
    pub fn hover(&self) -> gpui::Rgba {
        self.tint(if theme::light(self.surface) { 0.06 } else { 0.07 })
    }

    /// Whether a swatch of `color` shows on this chrome.
    #[must_use]
    pub fn shows(&self, color: u32) -> bool {
        theme::contrast(color, crate::material::over(self.surface, self.alpha, theme::background())) >= 1.3
    }
}

/// The chrome's target: the focused block's backdrop, fully when it fills
/// the window, else partly; the theme background without one.
#[must_use]
pub fn chrome_target(theme_bg: u32, backdrop: Option<u32>, alone: bool) -> u32 {
    match backdrop {
        Some(c) if alone => c,
        Some(c) => theme::mix(theme_bg, c, SHARED_MIX),
        None => theme_bg,
    }
}

/// An eased transition of the chrome surface toward a target colour.
pub struct Blend {
    from: u32,
    to: u32,
    started: Option<Instant>,
}

impl Blend {
    #[must_use]
    pub fn new(color: u32) -> Self {
        Self { from: color, to: color, started: None }
    }

    fn at(&self, now: Instant) -> (u32, bool) {
        match self.started.and_then(|started| crate::motion::progress(started, BLEND, now)) {
            Some(p) => (theme::mix(self.from, self.to, p), true),
            None => (self.to, false),
        }
    }

    /// Head for `target` (from wherever the blend is now) and return the
    /// colour at `now` and whether it is still moving.
    pub fn step(&mut self, target: u32, now: Instant, animate: bool) -> (u32, bool) {
        if target != self.to {
            self.from = self.at(now).0;
            self.to = target;
            self.started = animate.then_some(now);
        }
        let (color, moving) = self.at(now);
        if !moving {
            self.started = None;
        }
        (color, moving)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(columns: usize, color: impl Fn(usize, usize) -> u32) -> impl Fn(usize) -> u32 {
        move |i| color(i % columns, i / columns)
    }

    fn find(columns: usize, rows: usize, keep: Option<u32>, color: impl Fn(usize) -> u32) -> Option<u32> {
        dominant(columns, rows, keep, &mut Vec::new(), color)
    }

    #[test]
    fn uniform_and_empty_grids() {
        assert_eq!(find(80, 24, None, |_| 0x0f2027), Some(0x0f2027));
        assert_eq!(find(1, 1, None, |_| 5), Some(5));
        assert_eq!(find(0, 24, None, |_| 1), None);
        assert_eq!(find(80, 0, None, |_| 1), None);
    }

    #[test]
    fn samples_cover_edges_once() {
        let mut seen: Vec<usize> = samples(5, 4).map(|(i, _)| i).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "no cell sampled twice");
        assert_eq!(samples(5, 4).filter(|&(_, w)| w == 2).count(), 8, "both sides of every row");
    }

    #[test]
    fn side_columns_weigh_double() {
        // 4×3: sides 6 cells × 2 = 12 of 18. Half the cells, but the sides win.
        assert_eq!(find(4, 3, None, grid(4, |x, _| if x == 0 || x == 3 { 0xb } else { 0xa })), Some(0xb));
        assert_eq!(find(4, 3, None, grid(4, |x, _| if x == 0 || x == 3 { 0xa } else { 0xb })), Some(0xa));
    }

    #[test]
    fn bars_do_not_hide_the_backdrop() {
        let tui = grid(80, |_, y| if y == 0 || y == 23 { 0x333333 } else { 0x0f2027 });
        assert_eq!(find(80, 24, None, tui), Some(0x0f2027));
    }

    #[test]
    fn thresholds_and_ties() {
        // Two halves: a tie, neither reaches the share.
        assert_eq!(find(10, 10, None, grid(10, |x, _| if x < 5 { 1 } else { 2 })), None);
        // Three bands: no colour reaches the share.
        assert_eq!(find(9, 9, None, grid(9, |_, y| [1, 2, 3][y / 3])), None);
        // 10×10: every cell sampled, sides double: 120 in all, 12 per row.
        // Eight rows of colour 2 hold 96 (80%).
        assert_eq!(find(10, 10, None, grid(10, |_, y| if y < 8 { 2 } else { 3 })), Some(2));
    }

    #[test]
    fn hysteresis_keeps_a_backdrop_down_to_half() {
        // Six of ten rows: 72 / 120 = 60% adopts; five rows (50%) only keeps.
        let six = grid(10, |_, y| if y < 6 { 2 } else { 3 });
        let five = grid(10, |_, y| if y < 5 { 2 } else { 3 });
        let four = grid(10, |_, y| if y < 4 { 2 } else { 3 });
        assert_eq!(find(10, 10, None, &six), Some(2));
        assert_eq!(find(10, 10, None, &five), None, "50% does not adopt");
        assert_eq!(find(10, 10, Some(2), &five), Some(2), "50% keeps");
        assert_eq!(find(10, 10, Some(2), &four), Some(3), "40% drops; the other colour holds 60%");
        assert_eq!(find(10, 10, Some(7), &five), None, "a vanished colour is dropped");
    }

    #[test]
    fn large_grids_are_sampled_and_exit_early() {
        let calls = std::cell::Cell::new(0);
        let found = find(512, 256, None, |_| {
            calls.set(calls.get() + 1);
            7
        });
        assert_eq!(found, Some(7));
        let total = samples(512, 256).count();
        assert!(total < 3000, "{total} samples");
        assert!(calls.get() < total, "stops once the share is reached: {} of {total}", calls.get());
    }

    #[test]
    fn stable_needs_two_agreeing_samples() {
        let mut s = Stable::default();
        assert!(!s.feed(Some(1)));
        assert!(s.pending());
        assert!(s.feed(Some(1)));
        assert_eq!(s.value(), Some(1));
        assert!(!s.feed(None));
        assert!(!s.feed(Some(1)), "a flicker back cancels the change");
        assert!(!s.pending());
        assert!(!s.feed(Some(2)));
        assert!(!s.feed(Some(3)), "disagreeing samples restart");
        assert!(s.feed(Some(3)));
        assert_eq!(s.value(), Some(3));
    }

    #[test]
    fn chrome_matches_fully_only_when_alone() {
        assert_eq!(chrome_target(0x000000, Some(0x0000c8), true), 0x0000c8);
        assert_eq!(chrome_target(0x000000, Some(0x0000c8), false), theme::mix(0x000000, 0x0000c8, SHARED_MIX));
        assert_eq!(chrome_target(0x101010, None, false), 0x101010);
    }

    #[test]
    fn chrome_colours_read() {
        for surface in [0x00ff_ffff, 0x0080_8080, 0x003a_6ea5, 0x0000_0000, 0x00ff_00ff] {
            let c = Chrome::on(surface);
            assert!(theme::contrast(c.ink, surface) >= theme::TEXT_CONTRAST);
            assert!(theme::contrast(c.muted, surface) >= theme::TEXT_CONTRAST);
            assert!(theme::contrast(c.secondary, surface) >= theme::TEXT_CONTRAST);
            assert!(theme::contrast(c.icon, surface) >= theme::ICON_CONTRAST);
            assert!(theme::contrast(c.accent, surface) >= theme::ICON_CONTRAST);
        }
        for theme_bg in [0x0028_2c34, 0x00fa_fafa] {
            for surface in [0x00ff_ffff, 0x0080_8080, 0x003a_6ea5, 0x0000_0000, 0x00ff_00ff, 0x0013_2026, theme_bg] {
                {
                    let alpha = crate::material::readable_alpha(surface, crate::material::VEIL, theme_bg, &[]);
                    let c = Chrome::veiled_over(surface, alpha, theme_bg, 0x0061_afef);
                    assert!((c.alpha - alpha).abs() < f32::EPSILON || c.alpha >= 1.0);
                    let seen = crate::material::seen(surface, c.alpha, theme_bg);
                    for bg in if c.alpha < 1.0 { seen.to_vec() } else { vec![surface] } {
                        assert!(theme::contrast(c.ink, bg) >= theme::TEXT_CONTRAST, "ink on {bg:06x}");
                        assert!(theme::contrast(c.muted, bg) >= theme::TEXT_CONTRAST, "muted on {bg:06x}");
                        assert!(theme::contrast(c.secondary, bg) >= theme::TEXT_CONTRAST);
                        assert!(theme::contrast(c.icon, bg) >= theme::ICON_CONTRAST);
                        assert!(theme::contrast(c.accent, bg) >= theme::ICON_CONTRAST);
                    }
                }
            }
        }
        for surface in [0x0028_2c34, 0x0018_2d44, 0x00fa_fafa] {
            let veiled = Chrome::veiled_over(surface, crate::material::VEIL, 0x0028_2c34, 0x0061_afef);
            let floating = veiled.floating();
            assert!((floating.fill().a - 1.0).abs() < f32::EPSILON, "floating surfaces are never see-through");
            assert!(theme::contrast(floating.ink, floating.surface) >= theme::TEXT_CONTRAST);
            assert_eq!(floating.floating(), floating);
            assert_eq!(veiled.toward_floating(0.0), veiled);
            assert_eq!(veiled.toward_floating(1.0), floating);
            let half = veiled.toward_floating(0.5);
            assert!(half.alpha > veiled.alpha && half.alpha < 1.0);
            let looks = crate::material::look(half.surface, half.alpha);
            let between = |c: u32, a: u32, b: u32| {
                (0..3).all(|i| {
                    let ch = |v: u32| (v >> (8 * i)) & 0xff;
                    ch(c) + 1 >= ch(a).min(ch(b)) && ch(c) <= ch(a).max(ch(b)) + 1
                })
            };
            assert!(between(looks, crate::material::look(surface, veiled.alpha), floating.surface), "{looks:06x}");
        }
        let c = Chrome::on(0x0028_2c34);
        assert!(!c.shows(0x002a_2e36), "a swatch like the strip is not shown");
        assert!(c.shows(0x00f4_ecd8));
    }

    /// A mirror of a block whose server terminal runs another theme than
    /// this app's: it renders with this app's theme, and the backdrop is
    /// judged against that, for a shell, a full-screen app on the default
    /// background and one painting its own.
    #[test]
    fn backdrops_follow_the_local_theme_whatever_the_server_runs() {
        use silo_proto::{
            data::{ServerData, StartMode, Welcome},
            model::{TermSize, TerminalConfig},
        };
        let (dark, light) =
            (TerminalConfig::default(), TerminalConfig { background: 0x00fa_fafa, foreground: 0x0038_3a42, ..TerminalConfig::default() });
        for (server, local) in [(dark, light), (light, dark)] {
            let mut mirror = silo_client::Mirror::new();
            mirror.begin(&Welcome { epoch: 1, mode: StartMode::Snapshot, offset: 0, size: TermSize::new(40, 12) }).unwrap();
            mirror.apply(ServerData::Configured(server)).unwrap();
            let grid = silo_vt::GridSize { columns: 40, rows: 12, cell_width: 8, cell_height: 16 };
            mirror.apply(ServerData::SnapshotHead(silo_vt::TerminalCore::new(grid).unwrap().snapshot().unwrap())).unwrap();
            mirror.apply(ServerData::SnapshotDone).unwrap();
            mirror.set_local_config(local);
            assert_eq!((mirror.config(), mirror.render_config()), (Some(server), Some(local)));
            let mut offset = 0;
            let mut sample = |mirror: &mut silo_client::Mirror, bytes: &[u8]| {
                mirror.apply(ServerData::Output { offset, bytes: bytes.to_vec() }).unwrap();
                offset += bytes.len() as u64;
                let frame = mirror.terminal().unwrap().frame().unwrap();
                let default = mirror.render_config().unwrap().background;
                assert_eq!(frame.background, default, "renders with the local theme");
                terminal_backdrop(&frame, default, None, &mut Vec::new())
            };
            assert_eq!(sample(&mut mirror, b"$ ls\r\nfile\r\n"), None, "a shell has no backdrop");
            assert_eq!(sample(&mut mirror, b"\x1b[?1049h\x1b[2J\x1b[Htop-like"), None, "default cells match the card");
            assert_eq!(sample(&mut mirror, b"\x1b[48;2;110;110;120m\x1b[2J\x1b[Hbtop-like"), Some(0x006e_6e78), "a painted backdrop");
            // A theme switch keeps the program's colours.
            mirror.set_local_config(server);
            assert_eq!(sample(&mut mirror, b""), Some(0x006e_6e78));
        }
    }

    #[test]
    fn blend_eases_and_settles() {
        let t0 = Instant::now();
        let mut blend = Blend::new(0x000000);
        assert_eq!(blend.step(0x000000, t0, true), (0x000000, false));
        let (mid, moving) = blend.step(0xffffff, t0 + BLEND / 2, true);
        assert!(moving);
        assert_eq!(mid, 0x000000, "starts where it was");
        let (mid, _) = blend.step(0xffffff, t0 + BLEND, true);
        assert!(mid > 0x808080 && mid < 0xffffff, "past half way at half time (ease out): {mid:06x}");
        assert_eq!(blend.step(0xffffff, t0 + BLEND * 2, true), (0xffffff, false));
        assert_eq!(blend.step(0x123456, t0 + BLEND * 2, false), (0x123456, false), "reduced motion is instant");
    }
}
