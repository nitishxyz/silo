//! Zoom in the viewer, free of gpui: pictures and PDFs scale around the
//! pointer (pinch, ⌘+ ⌘- ⌘0), text changes its font size; a PDF's pages
//! laid out down the view, which of them show, and the pixels each is drawn
//! at.

use std::ops::Range;

/// A picture's or a PDF's zoom bounds (1 fits the view).
pub const MIN: f32 = 0.25;
pub const MAX: f32 = 8.0;
/// Text's font-size factor bounds.
pub const TEXT_MIN: f32 = 0.6;
pub const TEXT_MAX: f32 = 3.0;
/// ⌘+ / ⌘- stops.
const STEPS: &[f32] = &[0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0];
/// Space around a PDF's pages, and between them.
pub const PAD: f32 = 16.0;
pub const GAP: f32 = 12.0;
/// A page's bitmap stays under this many pixels (a deep zoom gets softer).
const MAX_PIXELS: f32 = 32_000_000.0;

/// `zoom` after a pinch step `delta` (gpui's: 0.1 is 10 % more), within
/// `min..=max`.
#[must_use]
pub fn pinched(zoom: f32, delta: f32, min: f32, max: f32) -> f32 {
    (zoom * (1.0 + delta).max(0.1)).clamp(min, max)
}

/// The next ⌘+ (`dir` > 0) or ⌘- stop from `zoom`; ⌘0 (`dir` 0) is 1.
#[must_use]
pub fn stepped(zoom: f32, dir: i32, min: f32, max: f32) -> f32 {
    let next = match dir.signum() {
        0 => 1.0,
        1 => STEPS.iter().copied().find(|s| *s > zoom * 1.01).unwrap_or(max),
        _ => STEPS.iter().rev().copied().find(|s| *s < zoom * 0.99).unwrap_or(min),
    };
    next.clamp(min, max)
}

/// The scroll along one axis that keeps the content under `pointer` (from
/// the view's edge) in place when content of length `content` in a view of
/// length `view`, scrolled by `scroll`, scales by `k`. Content shorter than
/// the view is centred in it.
#[must_use]
pub fn anchored(scroll: f32, pointer: f32, view: f32, content: f32, k: f32) -> f32 {
    let inset = |len: f32| ((view - len) / 2.0).max(0.0);
    let at = scroll + pointer - inset(content);
    let scaled = content * k;
    (at * k + inset(scaled) - pointer).clamp(0.0, (scaled - view).max(0.0))
}

/// The scale fitting a `w`×`h` picture in a `view_w`×`view_h` view, never
/// above 1.
#[must_use]
pub fn fit(w: f32, h: f32, view_w: f32, view_h: f32) -> f32 {
    if w <= 0.0 || h <= 0.0 {
        return 1.0;
    }
    (view_w / w).min(view_h / h).clamp(0.01, 1.0)
}

/// A PDF's pages down the view: each page's top, the points-to-view scale
/// and the whole content's size.
#[derive(Clone, Debug, PartialEq)]
pub struct Pages {
    pub tops: Vec<f32>,
    pub scale: f32,
    pub width: f32,
    pub height: f32,
}

/// Lay out `pages` (sizes in points) so the widest fills `view_w` at zoom 1.
#[must_use]
pub fn pages(pages: &[(f32, f32)], view_w: f32, zoom: f32) -> Pages {
    let widest = pages.iter().map(|p| p.0).fold(1.0_f32, f32::max);
    let scale = (view_w - 2.0 * PAD).max(80.0) / widest * zoom;
    let mut tops = Vec::with_capacity(pages.len());
    let mut y = PAD;
    for (_, h) in pages {
        tops.push(y);
        y += h * scale + GAP;
    }
    let height = if pages.is_empty() { 2.0 * PAD } else { y - GAP + PAD };
    Pages { tops, scale, width: widest * scale + 2.0 * PAD, height }
}

impl Pages {
    /// The pages showing between content heights `top` and `bottom`.
    #[must_use]
    pub fn visible(&self, sizes: &[(f32, f32)], top: f32, bottom: f32) -> Range<usize> {
        let start = self.tops.partition_point(|t| *t <= top).saturating_sub(1);
        let start = if sizes.get(start).is_some_and(|s| self.tops[start] + s.1 * self.scale < top) { start + 1 } else { start };
        let end = self.tops.partition_point(|t| *t < bottom);
        start.min(end)..end
    }
}

/// The bitmap width (pixels) for a page `w`×`h` points at `scale`, on a
/// display of `scale_factor`; capped at [`MAX_PIXELS`].
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn bitmap_width(w: f32, h: f32, scale: f32, scale_factor: f32) -> u32 {
    let (pw, ph) = (w * scale * scale_factor, h * scale * scale_factor);
    let shrink = (MAX_PIXELS / (pw * ph).max(1.0)).sqrt().min(1.0);
    (pw * shrink).round().max(1.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinch_and_steps_stay_in_bounds() {
        assert!((pinched(1.0, 0.1, MIN, MAX) - 1.1).abs() < 1e-6);
        assert_eq!(pinched(7.9, 0.5, MIN, MAX), MAX);
        assert_eq!(pinched(0.3, -0.5, MIN, MAX), MIN);
        assert_eq!(stepped(1.0, 1, MIN, MAX), 1.1);
        assert_eq!(stepped(1.0, -1, MIN, MAX), 0.9);
        assert_eq!(stepped(1.05, -1, MIN, MAX), 1.0, "between stops: the next one down");
        assert_eq!(stepped(3.7, 0, MIN, MAX), 1.0);
        assert_eq!(stepped(8.0, 1, MIN, MAX), MAX);
        assert_eq!(stepped(2.0, 1, TEXT_MIN, TEXT_MAX), 2.5);
        assert_eq!(stepped(0.6, -1, TEXT_MIN, TEXT_MAX), TEXT_MIN);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        // 1000 long, scrolled 200, pointer 100 into a 400 view: content point 300 doubles to 600.
        assert_eq!(anchored(200.0, 100.0, 400.0, 1000.0, 2.0), 500.0);
        // Content shorter than the view is centred: 200 in 400 starts at 100; the pointer at 150 is 50 in.
        // Doubled it is 400 long (no inset), 50 in becomes 100, under the pointer at 150: scrolled 0 (clamped).
        assert_eq!(anchored(0.0, 150.0, 400.0, 200.0, 2.0), 0.0);
        assert_eq!(anchored(0.0, 300.0, 400.0, 400.0, 2.0), 300.0);
        // Zooming out never scrolls past the end.
        assert_eq!(anchored(600.0, 0.0, 400.0, 1000.0, 0.5), 100.0);
    }

    #[test]
    fn fitting_never_upscales() {
        assert_eq!(fit(100.0, 50.0, 400.0, 400.0), 1.0);
        assert_eq!(fit(800.0, 400.0, 400.0, 400.0), 0.5);
        assert_eq!(fit(400.0, 800.0, 400.0, 400.0), 0.5);
    }

    #[test]
    fn pages_lay_out_and_show_what_is_in_view() {
        let sizes = [(600.0, 800.0), (600.0, 800.0), (300.0, 400.0)];
        let p = pages(&sizes, 632.0, 1.0);
        assert_eq!(p.scale, 1.0);
        assert_eq!(p.tops, [PAD, PAD + 800.0 + GAP, PAD + 2.0 * (800.0 + GAP)]);
        assert_eq!(p.height, PAD + 1600.0 + 2.0 * GAP + 400.0 + PAD);
        assert_eq!(p.width, 632.0);
        assert_eq!(p.visible(&sizes, 0.0, 500.0), 0..1);
        assert_eq!(p.visible(&sizes, 700.0, 900.0), 0..2);
        assert_eq!(p.visible(&sizes, 817.0, 820.0), 1..1, "only the gap between pages");
        assert_eq!(p.visible(&sizes, 1700.0, 9000.0), 2..3);
        let zoomed = pages(&sizes, 632.0, 2.0);
        assert_eq!((zoomed.scale, zoomed.width), (2.0, 1232.0));
    }

    #[test]
    fn bitmaps_follow_the_display_within_a_cap() {
        assert_eq!(bitmap_width(600.0, 800.0, 1.0, 2.0), 1200);
        let deep = bitmap_width(600.0, 800.0, 8.0, 2.0);
        assert!(deep < 9600 && (f64::from(deep) * f64::from(deep) * 4.0 / 3.0) <= 32_000_000.0 * 1.01);
    }
}
