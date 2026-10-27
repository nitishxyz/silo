//! Client-local entry/exit motion for tabs and panes.
//!
//! Animations are keyed by stable server ids (window and block ids), so a
//! refreshed `SessionView` never replays them: only ids that appear or
//! disappear between two views of the *same* session animate. The first
//! view after attach, reconnect or a session switch only records ids.
//! Progress is computed from wall time while rendering; the layout (and so
//! every terminal grid) is final from the first frame, motion is a clipped
//! offset or a width/opacity change on top of it.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{Bounds, Pixels, point, size};
use silo_proto::model::{Axis, Layout};
use silo_render::PreparedFrame;

pub const TAB_ENTER: Duration = Duration::from_millis(190);
pub const TAB_EXIT: Duration = Duration::from_millis(180);
pub const PANE_ENTER: Duration = Duration::from_millis(210);
pub const PANE_EXIT: Duration = Duration::from_millis(200);
pub const PANE_ZOOM: Duration = Duration::from_millis(220);
/// Frosted: a zoomed pane's turn, while whole, between opaque (over its
/// siblings) and its veil (nothing under it).
pub const ZOOM_SETTLE: Duration = Duration::from_millis(100);
pub const CLOSE_BUTTON: Duration = Duration::from_millis(140);

/// Cubic ease-out.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Eased progress of an animation started at `started`, or `None` once it
/// finished.
pub fn progress(started: Instant, duration: Duration, now: Instant) -> Option<f32> {
    let duration = duration.mul_f32(slowdown());
    let elapsed = now.saturating_duration_since(started);
    (elapsed < duration).then(|| ease_out(elapsed.as_secs_f32() / duration.as_secs_f32()))
}

/// Interface motion (agent block): expand/collapse, enter/leave, morphs.
pub const UI: Duration = Duration::from_millis(260);
pub const UI_FAST: Duration = Duration::from_millis(180);

/// `cubic-bezier(0.25, 1, 0.5, 1)`: the interface's ease-out.
pub fn ease_ui(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let (x1, y1, x2, y2) = (0.25_f32, 1.0_f32, 0.5_f32, 1.0_f32);
    let bez = |a: f32, b: f32, s: f32| 3.0 * a * s * (1.0 - s).powi(2) + 3.0 * b * s * s * (1.0 - s) + s.powi(3);
    // Solve x(s) = t by bisection (monotonic x), then y(s).
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if bez(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    bez(y1, y2, (lo + hi) / 2.0)
}

/// Eased (`ease_ui`) progress of an interface animation, `None` once done
/// or under Reduce Motion.
pub fn enter(started: Instant, duration: Duration, now: Instant, reduce: bool) -> Option<f32> {
    let duration = duration.mul_f32(slowdown());
    let elapsed = now.saturating_duration_since(started);
    (!reduce && elapsed < duration).then(|| ease_ui(elapsed.as_secs_f32() / duration.as_secs_f32()))
}

/// A two-state transition (open/closed, shown/hidden) that can reverse
/// mid-way: `value` eases from wherever it was toward 0 or 1.
#[derive(Clone, Copy, Debug)]
pub struct Toggle {
    on: bool,
    from: f32,
    at: Option<Instant>,
    duration: Duration,
}

impl Toggle {
    pub const fn new(on: bool, duration: Duration) -> Self {
        Self { on, from: if on { 1.0 } else { 0.0 }, at: None, duration }
    }

    pub const fn on(&self) -> bool {
        self.on
    }

    /// Turn on or off from the current value; no-op if already there.
    pub fn set(&mut self, on: bool, now: Instant, reduce: bool) {
        if self.on != on {
            self.from = self.value(now, reduce);
            self.on = on;
            self.at = Some(now);
        }
    }

    /// 0 (off) … 1 (on), eased.
    pub fn value(&self, now: Instant, reduce: bool) -> f32 {
        let target = if self.on { 1.0 } else { 0.0 };
        match self.at.and_then(|at| enter(at, self.duration, now, reduce)) {
            Some(p) => self.from + (target - self.from) * p,
            None => target,
        }
    }

    pub fn moving(&self, now: Instant, reduce: bool) -> bool {
        self.at.is_some_and(|at| enter(at, self.duration, now, reduce).is_some())
    }

    /// Off and done moving: nothing to draw.
    pub fn hidden(&self, now: Instant, reduce: bool) -> bool {
        !self.on && !self.moving(now, reduce)
    }
}

/// A height easing toward a target that changes over time (a measured
/// content height): each new target starts from the current value, so it
/// never jumps. The first target is taken as is.
#[derive(Clone, Copy, Debug)]
pub struct Glide {
    from: f32,
    to: Option<f32>,
    at: Option<Instant>,
    duration: Duration,
}

impl Glide {
    pub const fn new(duration: Duration) -> Self {
        Self { from: 0.0, to: None, at: None, duration }
    }

    /// Ease toward `to` from wherever it is; sub-pixel changes are ignored.
    pub fn target(&mut self, to: f32, now: Instant, reduce: bool) {
        match self.to {
            None => (self.from, self.to) = (to, Some(to)),
            Some(old) if (old - to).abs() >= 0.5 => {
                self.from = self.value(now, reduce);
                self.to = Some(to);
                self.at = Some(now);
            }
            Some(_) => {}
        }
    }

    pub fn value(&self, now: Instant, reduce: bool) -> f32 {
        let to = self.to.unwrap_or(self.from);
        match self.at.and_then(|at| enter(at, self.duration, now, reduce)) {
            Some(p) => self.from + (to - self.from) * p,
            None => to,
        }
    }

    pub fn moving(&self, now: Instant, reduce: bool) -> bool {
        self.at.is_some_and(|at| enter(at, self.duration, now, reduce).is_some())
    }
}

/// `SILO_MOTION_SLOWDOWN=10` stretches every animation, for inspecting
/// intermediate frames in UI tests.
pub fn slowdown() -> f32 {
    static FACTOR: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *FACTOR.get_or_init(|| std::env::var("SILO_MOTION_SLOWDOWN").ok().and_then(|v| v.parse().ok()).filter(|f: &f32| *f > 0.0).unwrap_or(1.0))
}

/// Ids added and removed since the previous sync of the same scope.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// The set of ids last seen for one scope (a session).
#[derive(Default)]
pub struct Tracker {
    scope: Option<String>,
    known: Vec<String>,
}

impl Tracker {
    /// Forget everything: the next sync only records (attach, reconnect).
    pub fn reset(&mut self) {
        self.scope = None;
        self.known.clear();
    }

    /// Record `ids` for `scope`, reporting changes only when the previous
    /// sync was for the same scope.
    pub fn sync(&mut self, scope: &str, ids: Vec<String>) -> Diff {
        let same = self.scope.as_deref() == Some(scope);
        let diff = if same {
            let old: HashSet<&String> = self.known.iter().collect();
            let new: HashSet<&String> = ids.iter().collect();
            Diff {
                added: ids.iter().filter(|id| !old.contains(id)).cloned().collect(),
                removed: self.known.iter().filter(|id| !new.contains(id)).cloned().collect(),
            }
        } else {
            Diff::default()
        };
        self.scope = Some(scope.to_owned());
        self.known = ids;
        diff
    }
}

/// Where a pane enters from or leaves towards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// Offset of the content as a fraction of the slot (x, y) for a pane
    /// that is `hidden` (1 = fully outside the slot) towards this edge.
    pub fn offset(self, hidden: f32) -> (f32, f32) {
        match self {
            Edge::Left => (-hidden, 0.0),
            Edge::Right => (hidden, 0.0),
            Edge::Top => (0.0, -hidden),
            Edge::Bottom => (0.0, hidden),
        }
    }
}

/// The outer edge a closing pane leaves through: the side of the body it
/// touches along the axis it did not span.
pub fn exit_edge(pane: Bounds<Pixels>, body: Bounds<Pixels>) -> Edge {
    let (px, py) = (f32::from(pane.origin.x), f32::from(pane.origin.y));
    let (pw, ph) = (f32::from(pane.size.width), f32::from(pane.size.height));
    let (bx, by) = (f32::from(body.origin.x), f32::from(body.origin.y));
    let (bw, bh) = (f32::from(body.size.width), f32::from(body.size.height));
    let width_share = pw / bw.max(1.0);
    let height_share = ph / bh.max(1.0);
    if width_share <= height_share {
        if px + pw / 2.0 < bx + bw / 2.0 { Edge::Left } else { Edge::Right }
    } else if py + ph / 2.0 < by + bh / 2.0 {
        Edge::Top
    } else {
        Edge::Bottom
    }
}

/// A closed tab, drawn collapsing at its old position.
pub struct TabGhost {
    pub id: String,
    pub title: String,
    pub icon: &'static str,
    pub index: usize,
    /// Its width as last drawn; it collapses from there.
    pub width: Option<f32>,
    pub started: Instant,
}

/// How a block's pane leaves the tiled layout (closed, floated, parked).
/// Every kind has one ([`crate::blocks::Kind::exit`]): none snaps away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Its last painted frame slides out (a terminal: drawing it live would
    /// resize its pty).
    Frame,
    /// The pane itself slides out, still drawing.
    Pane,
    /// Its card and header slide out: the pane is shown elsewhere now
    /// (floated, parked, moved), or there is none here.
    Card,
}

/// What a closing pane's ghost shows under its header.
pub enum GhostBody {
    Frame(Arc<PreparedFrame>),
    Pane(crate::blocks::BlockPane),
    Card,
}

/// A closed pane sliding out of its old slot. Visual only.
pub struct PaneGhost {
    pub body: GhostBody,
    pub slot: Bounds<Pixels>,
    pub edge: Edge,
    /// It floated: it shrinks a little and fades where it was
    /// ([`float_exit`]) instead of sliding out through `edge`.
    pub float: bool,
    /// The kind's glyph, in the header.
    pub icon: &'static str,
    /// Header label when pane headers were shown.
    pub title: Option<String>,
    /// The card's fill (its block's backdrop or the theme background).
    pub fill: u32,
    pub started: Instant,
}

/// A pane growing from its tiled slot to the whole pane area (zoom), or
/// shrinking back (unzoom). The terminal keeps the zoomed size until the
/// pane lands in its slot, so its grid resizes once. With a `settle`, the
/// whole pane then fades to its veil (zoom), or first turns opaque (unzoom).
pub struct Zoom {
    pub block: String,
    pub expanding: bool,
    pub started: Instant,
    pub settle: Duration,
}

impl Zoom {
    /// How far the pane covers the pane area (0 = its slot, 1 = all), and
    /// how opaque it is over its veil (1 while it moves over its siblings).
    pub fn frame(&self, now: Instant) -> Option<(f32, f32)> {
        let (moving, settle) = (PANE_ZOOM.mul_f32(slowdown()), self.settle.mul_f32(slowdown()));
        let t = now.saturating_duration_since(self.started);
        if t >= moving + settle {
            return None;
        }
        let part = |t: Duration, of: Duration| if of.is_zero() { 1.0 } else { (t.as_secs_f32() / of.as_secs_f32()).min(1.0) };
        // Near its slot it overlaps little, so it turns opaque gradually.
        let near_slot = |extent: f32| (extent * 6.0).min(1.0);
        Some(if self.expanding {
            let extent = ease_out(part(t, moving));
            (extent, if t < moving { near_slot(extent) } else { 1.0 - part(t - moving, settle) })
        } else {
            let extent = 1.0 - ease_out(part(t.saturating_sub(settle), moving));
            (extent, if t < settle { part(t, settle) } else { near_slot(extent) })
        })
    }
}

/// The panes of a window that stays shown which slide out (in `old`'s
/// tiled layout, `removed` from it) and in (`added`, along the axis of the
/// split now holding them). Kind plays no part: every block moves alike.
pub fn pane_moves(old: &Layout, new: &Layout, diff: &Diff) -> (Vec<String>, Vec<(String, Edge)>) {
    let old_blocks = old.blocks();
    let leaving = diff.removed.iter().filter(|id| old_blocks.contains(id)).cloned().collect();
    let entering = diff
        .added
        .iter()
        .filter_map(|id| match crate::layout::parent_axis(new, id) {
            Some(Axis::Horizontal) => Some((id.clone(), Edge::Right)),
            Some(Axis::Vertical) => Some((id.clone(), Edge::Bottom)),
            None => None,
        })
        .collect();
    (leaving, entering)
}

/// How small a closing float gets as it fades out.
pub const FLOAT_EXIT_SCALE: f32 = 0.94;

/// A closing float's scale and opacity `p` (eased, 0 … 1) through its exit:
/// it recedes in place, as a window closes, rather than sliding to an edge.
pub fn float_exit(p: f32) -> (f32, f32) {
    let p = p.clamp(0.0, 1.0);
    (1.0 - (1.0 - FLOAT_EXIT_SCALE) * p, 1.0 - p)
}

/// Window-space bounds from the last paint, for geometry the next frame
/// animates between.
#[derive(Clone, Default)]
pub struct Measured {
    /// The pane area: what a zoomed pane fills.
    pub area: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Each tiled pane's slot.
    pub cells: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Each float's card, as drawn.
    pub floats: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
}

/// `from` moved `t` of the way towards `to`.
pub fn lerp(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    let mix = |a: Pixels, b: Pixels| a + (b - a) * t;
    Bounds::new(
        point(mix(from.origin.x, to.origin.x), mix(from.origin.y, to.origin.y)),
        size(mix(from.size.width, to.size.width), mix(from.size.height, to.size.height)),
    )
}

/// All running motion of one workspace window.
#[derive(Default)]
pub struct Motion {
    pub windows: Tracker,
    pub blocks: Tracker,
    pub tab_enter: HashMap<String, Instant>,
    pub pane_enter: HashMap<String, (Instant, Edge)>,
    pub tab_ghosts: Vec<TabGhost>,
    pub pane_ghosts: Vec<PaneGhost>,
    pub zoom: Option<Zoom>,
    pub measured: Measured,
    hover: Option<(String, Instant)>,
    unhover: Option<(String, Instant)>,
}

impl Motion {
    /// Attach, reconnect or session switch: forget ids, drop running motion.
    pub fn reset(&mut self) {
        self.windows.reset();
        self.blocks.reset();
        self.tab_enter.clear();
        self.pane_enter.clear();
        self.tab_ghosts.clear();
        self.pane_ghosts.clear();
        self.zoom = None;
        self.measured.cells.borrow_mut().clear();
        self.measured.floats.borrow_mut().clear();
    }

    /// Drop finished animations; true while anything still moves.
    pub fn prune(&mut self, now: Instant) -> bool {
        self.tab_enter.retain(|_, s| progress(*s, TAB_ENTER, now).is_some());
        self.pane_enter.retain(|_, (s, _)| progress(*s, PANE_ENTER, now).is_some());
        self.tab_ghosts.retain(|g| progress(g.started, TAB_EXIT, now).is_some());
        self.pane_ghosts.retain(|g| progress(g.started, PANE_EXIT, now).is_some());
        if self.zoom.as_ref().is_some_and(|z| z.frame(now).is_none()) {
            self.zoom = None;
        }
        if self.unhover.as_ref().is_some_and(|(_, s)| progress(*s, CLOSE_BUTTON, now).is_none()) {
            self.unhover = None;
        }
        let hovering = self.hover.as_ref().is_some_and(|(_, s)| progress(*s, CLOSE_BUTTON, now).is_some());
        hovering
            || self.unhover.is_some()
            || !self.tab_enter.is_empty()
            || !self.pane_enter.is_empty()
            || !self.tab_ghosts.is_empty()
            || !self.pane_ghosts.is_empty()
            || self.zoom.is_some()
    }

    /// Drop what is kept for blocks that left the session.
    pub fn forget_blocks(&mut self, removed: &[String]) {
        self.measured.cells.borrow_mut().retain(|id, _| !removed.contains(id));
        if self.zoom.as_ref().is_some_and(|z| removed.contains(&z.block)) {
            self.zoom = None;
        }
    }

    /// Zoom (`expanding`) or unzoom `block`, continuing from where it is
    /// when it is already moving.
    pub fn start_zoom(&mut self, block: String, expanding: bool, settle: Duration, now: Instant) {
        let current = self.zoom.as_ref().filter(|z| z.block == block).and_then(|z| z.frame(now));
        let (moving, settled) = (PANE_ZOOM.mul_f32(slowdown()), settle.mul_f32(slowdown()));
        let elapsed = match current {
            None => Duration::ZERO,
            Some((extent, cover)) if extent >= 1.0 => {
                if expanding {
                    moving + settled.mul_f32(1.0 - cover)
                } else {
                    settled.mul_f32(cover)
                }
            }
            Some((extent, _)) => {
                if expanding {
                    moving.mul_f32(linear_for(extent))
                } else {
                    settled + moving.mul_f32(linear_for(1.0 - extent))
                }
            }
        };
        self.zoom = Some(Zoom { block, expanding, started: now.checked_sub(elapsed).unwrap_or(now), settle });
    }

    /// The running zoom animation's block and current clip rect within the
    /// pane area, once the pane area and the block's slot were measured.
    pub fn zoom_frame(&self, now: Instant) -> Option<(&str, Bounds<Pixels>, Bounds<Pixels>)> {
        let zoom = self.zoom.as_ref()?;
        let (extent, _) = zoom.frame(now)?;
        let area = self.measured.area.get()?;
        let slot = self.measured.cells.borrow().get(&zoom.block).copied()?;
        Some((zoom.block.as_str(), lerp(slot, area, extent), area))
    }

    /// The running zoom's extent and opacity (see [`Zoom::frame`]).
    pub fn zoom_state(&self, now: Instant) -> Option<(f32, f32)> {
        self.zoom.as_ref()?.frame(now)
    }

    /// Hover entered or left tab `id`. Leaving only affects the tab that
    /// is hovered, so states never leak between tabs.
    pub fn set_hover(&mut self, id: &str, hovered: bool, now: Instant, animate: bool) {
        let current = self.close_button(id, now);
        // Continue from the current extent instead of jumping.
        let start_at = |extent: f32| {
            let full = CLOSE_BUTTON.mul_f32(slowdown());
            let elapsed = if animate { full.mul_f32(linear_for(extent)) } else { full };
            now.checked_sub(elapsed).unwrap_or(now)
        };
        if hovered {
            let previous = self.hover.as_ref().filter(|(other, _)| other != id).map(|(other, _)| other.clone());
            if let Some(other) = previous {
                let extent = self.close_button(&other, now);
                self.hover = None;
                self.unhover = Some((other, start_at(1.0 - extent)));
            }
            if self.unhover.as_ref().is_some_and(|(u, _)| u == id) {
                self.unhover = None;
            }
            if self.hover.is_none() {
                self.hover = Some((id.to_owned(), start_at(current)));
            }
        } else if self.hover.as_ref().is_some_and(|(h, _)| h == id) {
            self.hover = None;
            self.unhover = Some((id.to_owned(), start_at(1.0 - current)));
        }
    }

    /// Extent (0 hidden … 1 shown) of tab `id`'s close button.
    pub fn close_button(&self, id: &str, now: Instant) -> f32 {
        match (&self.hover, &self.unhover) {
            (Some((h, s)), _) if h == id => progress(*s, CLOSE_BUTTON, now).unwrap_or(1.0),
            (_, Some((u, s))) if u == id => 1.0 - progress(*s, CLOSE_BUTTON, now).unwrap_or(1.0),
            _ => 0.0,
        }
    }
}

/// Inverse of `ease_out`: the linear time at which it reaches `value`.
fn linear_for(value: f32) -> f32 {
    1.0 - (1.0 - value.clamp(0.0, 1.0)).cbrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn ui_ease_and_toggle() {
        assert!(ease_ui(0.0).abs() < 1e-3 && (ease_ui(1.0) - 1.0).abs() < 1e-3);
        assert!(ease_ui(0.25) > 0.6, "front-loaded");
        let now = Instant::now();
        let mut t = Toggle::new(false, UI);
        assert!(t.hidden(now, false));
        t.set(true, now, false);
        assert!(t.value(now, false) < 0.01 && t.moving(now, false));
        assert!((t.value(now + Duration::from_secs(60), false) - 1.0).abs() < 1e-6);
        assert!((t.value(now, true) - 1.0).abs() < 1e-6, "reduce motion: instant");
    }

    #[test]
    fn glide_eases_from_where_it_is() {
        let now = Instant::now();
        let mut g = Glide::new(UI);
        g.target(18.0, now, false);
        assert!((g.value(now, false) - 18.0).abs() < 1e-6 && !g.moving(now, false), "first target: no motion");
        g.target(36.0, now, false);
        assert!((g.value(now, false) - 18.0).abs() < 1e-3 && g.moving(now, false), "starts from the old height");
        let mid = now + UI / 2;
        let at_mid = g.value(mid, false);
        assert!(at_mid > 18.0 && at_mid < 36.0);
        // A new target mid-way continues from the current value.
        g.target(0.0, mid, false);
        assert!((g.value(mid, false) - at_mid).abs() < 1e-3);
        assert!(g.value(mid + UI, false).abs() < 1e-6);
        g.target(0.2, mid + UI, false);
        assert!(!g.moving(mid + UI, false), "sub-pixel changes do not move");
        g.target(50.0, mid + UI, true);
        assert!((g.value(mid + UI, true) - 50.0).abs() < 1e-6, "reduce motion: instant");
    }

    #[test]
    fn first_sync_and_scope_change_do_not_animate() {
        let mut t = Tracker::default();
        assert_eq!(t.sync("s1", ids(&["a", "b"])), Diff::default());
        assert_eq!(t.sync("s2", ids(&["c"])), Diff::default());
        t.reset();
        assert_eq!(t.sync("s2", ids(&["c", "d"])), Diff::default());
    }

    #[test]
    fn only_new_ids_animate_and_refresh_does_not_replay() {
        let mut t = Tracker::default();
        t.sync("s", ids(&["a"]));
        assert_eq!(t.sync("s", ids(&["a", "b"])), Diff { added: ids(&["b"]), removed: vec![] });
        // Same ids again (title change, focus change, …): nothing.
        assert_eq!(t.sync("s", ids(&["a", "b"])), Diff::default());
        assert_eq!(t.sync("s", ids(&["b"])), Diff { added: vec![], removed: ids(&["a"]) });
    }

    #[test]
    fn progress_eases_and_ends() {
        let start = Instant::now();
        assert_eq!(progress(start, TAB_ENTER, start), Some(0.0));
        let mid = progress(start, TAB_ENTER, start + TAB_ENTER / 2).unwrap();
        assert!(mid > 0.5 && mid < 1.0, "ease-out is past half at half time: {mid}");
        assert_eq!(progress(start, TAB_ENTER, start + TAB_ENTER), None);
    }

    #[test]
    fn exit_edge_follows_geometry() {
        let body = Bounds::new(point(px(0.), px(46.)), size(px(1000.), px(600.)));
        let right = Bounds::new(point(px(500.), px(46.)), size(px(500.), px(600.)));
        let left = Bounds::new(point(px(0.), px(46.)), size(px(500.), px(600.)));
        let bottom = Bounds::new(point(px(0.), px(346.)), size(px(1000.), px(300.)));
        let top = Bounds::new(point(px(0.), px(46.)), size(px(1000.), px(300.)));
        assert_eq!(exit_edge(right, body), Edge::Right);
        assert_eq!(exit_edge(left, body), Edge::Left);
        assert_eq!(exit_edge(bottom, body), Edge::Bottom);
        assert_eq!(exit_edge(top, body), Edge::Top);
    }

    #[test]
    fn close_button_hover_does_not_leak_between_tabs() {
        let mut m = Motion::default();
        let t0 = Instant::now();
        m.set_hover("a", true, t0, true);
        let done = t0 + CLOSE_BUTTON;
        assert!((m.close_button("a", done) - 1.0).abs() < 1e-3);
        assert_eq!(m.close_button("b", done), 0.0);
        // Entering b before a's leave event: a collapses, b grows.
        m.set_hover("b", true, done, true);
        // A stale leave for a must not hide b.
        m.set_hover("a", false, done, true);
        let later = done + CLOSE_BUTTON;
        assert!((m.close_button("b", later) - 1.0).abs() < 1e-3);
        assert_eq!(m.close_button("a", later), 0.0);
        m.set_hover("b", false, later, true);
        assert!(m.close_button("b", later) > 0.99, "collapse starts from fully shown");
        assert_eq!(m.close_button("b", later + CLOSE_BUTTON), 0.0);
        assert!(!m.prune(later + CLOSE_BUTTON));
    }

    #[test]
    fn reduced_motion_close_button_is_instant() {
        let mut m = Motion::default();
        let t0 = Instant::now() + Duration::from_secs(1);
        m.set_hover("a", true, t0, false);
        assert_eq!(m.close_button("a", t0), 1.0);
    }

    #[test]
    fn zoom_reverses_from_its_current_extent() {
        let mut m = Motion::default();
        let t0 = Instant::now() + Duration::from_secs(1);
        m.start_zoom("a".into(), true, Duration::ZERO, t0);
        assert!(m.zoom_frame(t0).is_none(), "nothing to draw before the slot was measured");
        let slot = Bounds::new(point(px(500.), px(0.)), size(px(500.), px(600.)));
        let area = Bounds::new(point(px(0.), px(0.)), size(px(1000.), px(600.)));
        m.measured.area.set(Some(area));
        m.measured.cells.borrow_mut().insert("a".into(), slot);
        assert_eq!(m.zoom_frame(t0), Some(("a", slot, area)));
        let mid = t0 + PANE_ZOOM / 2;
        let (_, rect, _) = m.zoom_frame(mid).unwrap();
        m.start_zoom("a".into(), false, Duration::ZERO, mid);
        let (_, reversed, _) = m.zoom_frame(mid).unwrap();
        assert!(f32::from((reversed.origin.x - rect.origin.x).abs()) < 0.5, "unzoom starts where zoom was");
        assert!(m.prune(mid));
        assert!(!m.prune(mid + PANE_ZOOM));
        assert!(m.zoom.is_none());
        m.start_zoom("a".into(), true, Duration::ZERO, mid);
        m.forget_blocks(&["a".into()]);
        assert!(m.zoom.is_none() && m.measured.cells.borrow().is_empty());
    }

    #[test]
    fn frosted_zoom_settles_while_whole() {
        let mut m = Motion::default();
        let t0 = Instant::now() + Duration::from_secs(1);
        m.start_zoom("a".into(), true, ZOOM_SETTLE, t0);
        assert_eq!(m.zoom_state(t0), Some((0.0, 0.0)), "a veil in its slot");
        let (extent, cover) = m.zoom_state(t0 + PANE_ZOOM / 2).unwrap();
        assert!(extent < 1.0 && (cover - 1.0).abs() < f32::EPSILON, "opaque over its siblings");
        let settling = t0 + PANE_ZOOM + ZOOM_SETTLE / 2;
        let (extent, cover) = m.zoom_state(settling).unwrap();
        assert!((extent - 1.0).abs() < f32::EPSILON && cover > 0.0 && cover < 1.0, "fades once whole");
        m.start_zoom("a".into(), false, ZOOM_SETTLE, settling);
        let (extent, reversed) = m.zoom_state(settling).unwrap();
        assert!((extent - 1.0).abs() < f32::EPSILON && (reversed - cover).abs() < 1e-3, "unzoom continues the fade");
        let (extent, cover) = m.zoom_state(settling + ZOOM_SETTLE).unwrap();
        assert!(extent < 1.0 && cover > 0.99, "turns opaque before it shrinks");
        assert!(m.zoom_state(settling + ZOOM_SETTLE + PANE_ZOOM).is_none());
    }

    #[test]
    fn every_kind_slides_in_and_out() {
        use crate::blocks::Kind;
        let block = |id: &str| Layout::Block { id: id.into() };
        let alone = block("base");
        let start = Instant::now();
        for kind in Kind::ALL {
            let id = format!("{kind:?}");
            let split = Layout::Split { axis: Axis::Horizontal, ratio: 0.5, first: Box::new(block("base")), second: Box::new(block(&id)) };
            let mut t = Tracker::default();
            t.sync("s", alone.blocks());
            // Opened: it slides in from the side it splits off to.
            let opened = t.sync("s", split.blocks());
            assert_eq!(pane_moves(&alone, &split, &opened), (vec![], vec![(id.clone(), Edge::Right)]), "{kind:?} opens");
            // Closed: it slides out, showing what it showed.
            let closed = t.sync("s", alone.blocks());
            assert_eq!(pane_moves(&split, &alone, &closed), (vec![id.clone()], vec![]), "{kind:?} closes");
            let exit = kind.exit(false);
            assert_eq!(matches!(exit, Exit::Frame | Exit::Pane), kind != Kind::Unsupported, "{kind:?} leaves as {exit:?}");
            // Floated or parked, its pane is drawn there: never twice.
            assert_ne!(kind.exit(true), Exit::Pane, "{kind:?}");
            let mut m = Motion::default();
            let slot = Bounds::new(point(px(500.), px(0.)), size(px(500.), px(600.)));
            m.pane_ghosts.push(PaneGhost {
                body: GhostBody::Card,
                slot,
                edge: Edge::Right,
                float: false,
                icon: kind.icon(),
                title: None,
                fill: 0,
                started: start,
            });
            m.pane_enter.insert(id, (start, Edge::Right));
            assert!(m.prune(start + PANE_EXIT / 2), "{kind:?} moves");
            assert!(!m.prune(start + PANE_ENTER.max(PANE_EXIT)), "{kind:?} settles");
        }
    }

    #[test]
    fn a_closing_float_recedes_and_fades_in_place() {
        assert_eq!(float_exit(0.0), (1.0, 1.0), "starts as it was");
        let (scale, alpha) = float_exit(0.5);
        assert!(scale < 1.0 && scale > FLOAT_EXIT_SCALE && (alpha - 0.5).abs() < 1e-6);
        assert_eq!(float_exit(1.0), (FLOAT_EXIT_SCALE, 0.0), "gone, slightly smaller");
        assert_eq!(float_exit(2.0), float_exit(1.0));
    }

    #[test]
    fn lerp_moves_between_bounds() {
        let a = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(50.)));
        let b = Bounds::new(point(px(100.), px(50.)), size(px(300.), px(150.)));
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        assert_eq!(lerp(a, b, 0.5), Bounds::new(point(px(50.), px(25.)), size(px(200.), px(100.))));
    }
}
