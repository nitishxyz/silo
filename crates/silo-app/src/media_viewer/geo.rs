//! Where the viewer's image or video sits and how it zooms and pans: pure, apart from drawing.

use super::player;

/// Zoom bounds (1 is 100 %).
pub const MIN: f32 = 0.1;
pub const MAX: f32 = 8.0;
/// Room for the toolbar above the fitted image, the footer below it, and the sides.
pub(super) const TOP: f32 = 56.0;
pub(super) const BOTTOM: f32 = 84.0;
pub(super) const SIDE: f32 = 32.0;
/// A video's room above it: its title and Close.
pub(super) const VIDEO_TOP: f32 = 64.0;

/// Where the image sits: the view's size, the fitted area's centre (both local), and the fit scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geo {
    pub view: (f32, f32),
    pub center: (f32, f32),
    pub fit: f32,
    pub dims: (f32, f32),
}

impl Geo {
    #[must_use]
    pub fn new(view: (f32, f32), dims: (f32, f32)) -> Self {
        let area = ((view.0 - 2.0 * SIDE).max(40.0), (view.1 - TOP - BOTTOM).max(40.0));
        let fit = if dims.0 > 0.0 && dims.1 > 0.0 { (area.0 / dims.0).min(area.1 / dims.1).min(1.0) } else { 1.0 };
        Self { view, center: (view.0 / 2.0, TOP + area.1 / 2.0), fit, dims }
    }

    /// A video: fit to the area above the player's bar (`fill`: the whole view), larger than its pixels too.
    #[must_use]
    pub fn video(view: (f32, f32), dims: (f32, f32), fill: bool) -> Self {
        let (top, bottom, side) = if fill { (0.0, 0.0, 0.0) } else { (VIDEO_TOP, player::BAR_ROOM, SIDE) };
        let area = ((view.0 - 2.0 * side).max(40.0), (view.1 - top - bottom).max(40.0));
        let fit = if dims.0 > 0.0 && dims.1 > 0.0 { (area.0 / dims.0).min(area.1 / dims.1) } else { 1.0 };
        Self { view, center: (view.0 / 2.0, top + area.1 / 2.0), fit, dims }
    }

    /// `pan` kept so an image larger than the view covers it (else centred on that axis).
    #[must_use]
    pub fn clamp(&self, scale: f32, pan: (f32, f32)) -> (f32, f32) {
        let axis = |pan: f32, len: f32, view: f32, center: f32| {
            if len <= view { 0.0 } else { pan.clamp(view - len / 2.0 - center, len / 2.0 - center) }
        };
        (axis(pan.0, self.dims.0 * scale, self.view.0, self.center.0), axis(pan.1, self.dims.1 * scale, self.view.1, self.center.1))
    }

    /// The pan after scaling `from` → `to` with the point `at` (local) staying put.
    #[must_use]
    pub fn anchored(&self, from: f32, to: f32, pan: (f32, f32), at: (f32, f32)) -> (f32, f32) {
        let q = (at.0 - self.center.0, at.1 - self.center.1);
        let k = to / from.max(0.0001);
        self.clamp(to, (q.0 - (q.0 - pan.0) * k, q.1 - (q.1 - pan.1) * k))
    }

    /// The image is larger than the view on some axis (scrolling pans it).
    #[must_use]
    pub fn overflows(&self, scale: f32) -> bool {
        self.dims.0 * scale > self.view.0 + 0.5 || self.dims.1 * scale > self.view.1 + 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_without_upscaling_and_centres_what_fits() {
        let geo = Geo::new((800.0, 600.0), (1024.0, 1024.0));
        assert!((geo.fit - (600.0 - TOP - BOTTOM) / 1024.0).abs() < 1e-4);
        assert!((Geo::new((800.0, 600.0), (100.0, 50.0)).fit - 1.0).abs() < 1e-6, "never above 100 %");
        assert_eq!(geo.clamp(geo.fit, (50.0, -30.0)), (0.0, 0.0), "a fitted image stays centred");
        assert!(!geo.overflows(geo.fit) && geo.overflows(2.0));
    }

    #[test]
    fn a_video_fits_above_the_bar_or_fills_the_pane() {
        let geo = Geo::video((800.0, 600.0), (1280.0, 720.0), false);
        let h = 720.0 * geo.fit;
        assert!(geo.center.1 + h / 2.0 <= 600.0 - player::BAR_ROOM + 0.01, "clear of the bar");
        assert!(geo.center.1 - h / 2.0 >= VIDEO_TOP - 0.01, "clear of the title");
        let fill = Geo::video((800.0, 600.0), (1280.0, 720.0), true);
        assert!((fill.fit - 800.0 / 1280.0).abs() < 1e-5 && fill.center == (400.0, 300.0), "the whole pane, centred");
        assert!(Geo::video((800.0, 600.0), (64.0, 36.0), true).fit > 1.0, "upscaled");
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let geo = Geo::new((800.0, 600.0), (1000.0, 1000.0));
        let at = (geo.center.0 + 100.0, geo.center.1 + 50.0);
        let pan = geo.anchored(1.0, 2.0, (0.0, 0.0), at);
        // The image point under the pointer, before and after: (at - centre - pan) / scale.
        let before = ((at.0 - geo.center.0) / 1.0, (at.1 - geo.center.1) / 1.0);
        let after = ((at.0 - geo.center.0 - pan.0) / 2.0, (at.1 - geo.center.1 - pan.1) / 2.0);
        assert_eq!(before, after);
        // Panned past an edge, it stops there: the image still covers the view.
        let far = geo.clamp(2.0, (5000.0, -5000.0));
        assert_eq!(far, (1000.0 - geo.center.0, 600.0 - 1000.0 - geo.center.1));
    }
}
