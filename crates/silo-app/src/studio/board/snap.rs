//! Snapping while moving or resizing: to other nodes' edges and centres, and to frames' edges and their
//! padded insides. Lines are gathered once per gesture; each move only compares against them.

use super::geom::{Point, Rect};
use super::layout::{FRAME_PAD, FRAME_TITLE};

/// Snapping reaches this many view points.
pub const SNAP_VIEW: f64 = 6.0;

/// A snap line to draw: vertical (at `x` = `at`) or horizontal, spanning `from`..`to` on the other axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub at: f64,
    pub from: f64,
    pub to: f64,
}

/// A line something can snap to: its position and its extent on the other axis.
#[derive(Clone, Copy, Debug)]
struct Line {
    at: f64,
    lo: f64,
    hi: f64,
}

/// The lines of the nodes not being moved.
#[derive(Clone, Debug, Default)]
pub struct Snapper {
    xs: Vec<Line>,
    ys: Vec<Line>,
}

impl Snapper {
    /// From `rects` (`true`: a frame, which adds its padded inside).
    #[must_use]
    pub fn new(rects: impl IntoIterator<Item = (Rect, bool)>) -> Self {
        let mut s = Self::default();
        for (r, frame) in rects {
            let (v, h) = ((r.y, r.bottom()), (r.x, r.right()));
            s.xs.extend([r.x, r.center().x, r.right()].map(|at| Line { at, lo: v.0, hi: v.1 }));
            s.ys.extend([r.y, r.center().y, r.bottom()].map(|at| Line { at, lo: h.0, hi: h.1 }));
            if frame {
                s.xs.extend([r.x + FRAME_PAD, r.right() - FRAME_PAD].map(|at| Line { at, lo: v.0, hi: v.1 }));
                s.ys.extend([r.y + FRAME_TITLE + FRAME_PAD, r.bottom() - FRAME_PAD].map(|at| Line { at, lo: h.0, hi: h.1 }));
            }
        }
        s
    }

    /// The nearest line to any of `cands` within `tol`: the shift onto it and the line.
    fn nearest(lines: &[Line], cands: &[f64], tol: f64) -> Option<(f64, Line)> {
        let mut best: Option<(f64, Line)> = None;
        for l in lines {
            for &c in cands {
                let d = l.at - c;
                if d.abs() <= tol && best.is_none_or(|(b, _)| d.abs() < b.abs()) {
                    best = Some((d, *l));
                }
            }
        }
        best
    }

    /// Snaps rect `r` (moving): the shift to add and the guides to show.
    #[must_use]
    pub fn snap_rect(&self, r: Rect, tol: f64) -> (Point, Vec<Guide>) {
        let mut guides = Vec::new();
        let sx = Self::nearest(&self.xs, &[r.x, r.center().x, r.right()], tol);
        let sy = Self::nearest(&self.ys, &[r.y, r.center().y, r.bottom()], tol);
        let d = Point::new(sx.map_or(0.0, |s| s.0), sy.map_or(0.0, |s| s.0));
        let moved = r.translate(d);
        if let Some((_, l)) = sx {
            guides.push(Guide { vertical: true, at: l.at, from: l.lo.min(moved.y), to: l.hi.max(moved.bottom()) });
        }
        if let Some((_, l)) = sy {
            guides.push(Guide { vertical: false, at: l.at, from: l.lo.min(moved.x), to: l.hi.max(moved.right()) });
        }
        (d, guides)
    }

    /// Snaps a point (a resize corner): the snapped point and the guides.
    #[must_use]
    pub fn snap_point(&self, p: Point, tol: f64) -> (Point, Vec<Guide>) {
        let mut guides = Vec::new();
        let sx = Self::nearest(&self.xs, &[p.x], tol);
        let sy = Self::nearest(&self.ys, &[p.y], tol);
        let q = Point::new(p.x + sx.map_or(0.0, |s| s.0), p.y + sy.map_or(0.0, |s| s.0));
        if let Some((_, l)) = sx {
            guides.push(Guide { vertical: true, at: l.at, from: l.lo.min(q.y), to: l.hi.max(q.y) });
        }
        if let Some((_, l)) = sy {
            guides.push(Guide { vertical: false, at: l.at, from: l.lo.min(q.x), to: l.hi.max(q.x) });
        }
        (q, guides)
    }
}
