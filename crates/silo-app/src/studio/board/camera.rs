//! The camera (pan and zoom), canvas ↔ view transforms, fitting, and the minimap.

use super::geom::{Point, Rect, Size};

/// Zoom limits (5 %–400 %).
pub const MIN_ZOOM: f64 = 0.05;
pub const MAX_ZOOM: f64 = 4.0;
/// Fitting never zooms in past 100 %.
pub const FIT_MAX_ZOOM: f64 = 1.0;
/// Room left around what is fitted, in view points.
pub const FIT_PADDING: f64 = 64.0;
/// How fast `⌘`-scroll zooms: the factor is `exp(-dy * ZOOM_SPEED)`.
pub const ZOOM_SPEED: f64 = 0.01;

#[must_use]
pub fn clamp_zoom(z: f64) -> f64 {
    if z.is_finite() { z.clamp(MIN_ZOOM, MAX_ZOOM) } else { 1.0 }
}

/// Where the canvas is: a canvas point `p` shows at `p * zoom + pan` in the view (view points, origin at the
/// board's top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub pan: Point,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self { pan: Point::ZERO, zoom: 1.0 }
    }
}

impl Camera {
    #[must_use]
    pub fn to_view(self, p: Point) -> Point {
        Point::new(p.x * self.zoom + self.pan.x, p.y * self.zoom + self.pan.y)
    }

    #[must_use]
    pub fn to_canvas(self, v: Point) -> Point {
        Point::new((v.x - self.pan.x) / self.zoom, (v.y - self.pan.y) / self.zoom)
    }

    #[must_use]
    pub fn rect_to_view(self, r: Rect) -> Rect {
        let o = self.to_view(r.origin());
        Rect::new(o.x, o.y, r.w * self.zoom, r.h * self.zoom)
    }

    #[must_use]
    pub fn rect_to_canvas(self, r: Rect) -> Rect {
        let o = self.to_canvas(r.origin());
        Rect::new(o.x, o.y, r.w / self.zoom, r.h / self.zoom)
    }

    /// The canvas rect a view of `size` shows (for culling).
    #[must_use]
    pub fn visible(self, size: Size) -> Rect {
        self.rect_to_canvas(Rect::new(0.0, 0.0, size.w, size.h))
    }

    /// Panned by `d` view points (the content moves with `d`).
    #[must_use]
    pub fn panned(self, d: Point) -> Self {
        Self { pan: self.pan + d, zoom: self.zoom }
    }

    /// Zoomed to `zoom` (clamped) keeping the canvas point under view point `at` where it is.
    #[must_use]
    pub fn zoomed_at(self, at: Point, zoom: f64) -> Self {
        let zoom = clamp_zoom(zoom);
        let c = self.to_canvas(at);
        Self { pan: Point::new(at.x - c.x * zoom, at.y - c.y * zoom), zoom }
    }

    /// Centred on canvas point `c` in a view of `size`, at the same zoom.
    #[must_use]
    pub fn centered_on(self, c: Point, size: Size) -> Self {
        Self { pan: Point::new(size.w / 2.0 - c.x * self.zoom, size.h / 2.0 - c.y * self.zoom), zoom: self.zoom }
    }

    /// The camera showing all of `rect` in a view of `size`, `padding` view points around it, centred, never
    /// past [`FIT_MAX_ZOOM`]. An empty view or rect: 100 % centred on the rect's centre.
    #[must_use]
    pub fn fit(rect: Rect, size: Size, padding: f64) -> Self {
        let avail = Size::new((size.w - 2.0 * padding).max(1.0), (size.h - 2.0 * padding).max(1.0));
        let zoom = if size.w <= 0.0 || size.h <= 0.0 {
            1.0
        } else {
            clamp_zoom((avail.w / rect.w.max(1.0)).min(avail.h / rect.h.max(1.0)).min(FIT_MAX_ZOOM))
        };
        Self { pan: Point::ZERO, zoom }.centered_on(rect.center(), size)
    }

    /// The smallest pan that brings `rect` into a view of `size` (`margin` view points in), or `self` when it
    /// is already visible: for revealing a node without jumping.
    #[must_use]
    pub fn reveal(self, rect: Rect, size: Size, margin: f64) -> Self {
        let r = self.rect_to_view(rect);
        let shift = |lo: f64, hi: f64, max: f64| {
            if hi - lo > max - 2.0 * margin || lo < margin {
                margin - lo
            } else if hi > max - margin {
                max - margin - hi
            } else {
                0.0
            }
        };
        self.panned(Point::new(shift(r.x, r.right(), size.w), shift(r.y, r.bottom(), size.h)))
    }
}

/// The minimap's geometry: the content and the view's rect scaled into `frame` (view points, where the
/// minimap is drawn).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Minimap {
    pub frame: Rect,
    /// The canvas rect it shows (content ∪ the visible rect, padded).
    pub world: Rect,
    /// Canvas → minimap scale.
    pub scale: f64,
    /// Where the minimap's drawing of `world` starts inside `frame` (it is centred).
    pub offset: Point,
}

impl Minimap {
    /// Shown only when the content is not wholly in view (`None` otherwise, or with no content).
    #[must_use]
    pub fn new(frame: Rect, content: Option<Rect>, visible: Rect) -> Option<Self> {
        let content = content?;
        if visible.contains_rect(content) || frame.w <= 1.0 || frame.h <= 1.0 {
            return None;
        }
        let world = content.union(visible);
        let world = world.outset(world.w.max(world.h) * 0.04);
        let scale = (frame.w / world.w.max(1.0)).min(frame.h / world.h.max(1.0));
        let offset = Point::new(frame.x + (frame.w - world.w * scale) / 2.0, frame.y + (frame.h - world.h * scale) / 2.0);
        Some(Self { frame, world, scale, offset })
    }

    /// A canvas rect (a node) in view points inside the minimap.
    #[must_use]
    pub fn rect(&self, r: Rect) -> Rect {
        Rect::new(
            self.offset.x + (r.x - self.world.x) * self.scale,
            self.offset.y + (r.y - self.world.y) * self.scale,
            r.w * self.scale,
            r.h * self.scale,
        )
    }

    /// The canvas point under view point `v` in the minimap.
    #[must_use]
    pub fn to_canvas(self, v: Point) -> Point {
        Point::new(self.world.x + (v.x - self.offset.x) / self.scale, self.world.y + (v.y - self.offset.y) / self.scale)
    }

    /// A click or drag at view point `v` on the minimap: the camera centred there.
    #[must_use]
    pub fn camera_at(&self, camera: Camera, v: Point, size: Size) -> Camera {
        camera.centered_on(self.to_canvas(v), size)
    }
}
