//! Plain geometry in `f64`: points, sizes, rects, corners. Canvas units unless a name says view.

use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn dist(self, o: Self) -> f64 {
        (self.x - o.x).hypot(self.y - o.y)
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn len(self) -> f64 {
        self.x.hypot(self.y)
    }
}

impl Add for Point {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Point {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f64> for Point {
    type Output = Self;
    fn mul(self, k: f64) -> Self {
        Self::new(self.x * k, self.y * k)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

impl Size {
    #[must_use]
    pub const fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }
}

/// `x`, `y` the top-left; `w`, `h` never negative once [`Rect::from_points`] or [`Rect::normalized`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    #[must_use]
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// The rect spanned by two corners, in any order.
    #[must_use]
    pub fn from_points(a: Point, b: Point) -> Self {
        Self::new(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
    }

    /// Centred on `c`.
    #[must_use]
    pub fn around(c: Point, w: f64, h: f64) -> Self {
        Self::new(c.x - w / 2.0, c.y - h / 2.0, w, h)
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        Self::from_points(self.origin(), Point::new(self.x + self.w, self.y + self.h))
    }

    #[must_use]
    pub fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    #[must_use]
    pub fn right(self) -> f64 {
        self.x + self.w
    }

    #[must_use]
    pub fn bottom(self) -> f64 {
        self.y + self.h
    }

    #[must_use]
    pub fn center(self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn size(self) -> Size {
        Size::new(self.w, self.h)
    }

    #[must_use]
    pub fn area(self) -> f64 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    #[must_use]
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    /// `o` lies wholly inside.
    #[must_use]
    pub fn contains_rect(self, o: Self) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }

    #[must_use]
    pub fn intersects(self, o: Self) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }

    #[must_use]
    pub fn union(self, o: Self) -> Self {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Self::new(x, y, self.right().max(o.right()) - x, self.bottom().max(o.bottom()) - y)
    }

    /// Grown by `d` on every side (shrunk when negative).
    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn inset(self, d: f64) -> Self {
        Self::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }

    #[must_use]
    pub fn outset(self, d: f64) -> Self {
        Self::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    #[must_use]
    pub fn translate(self, d: Point) -> Self {
        Self::new(self.x + d.x, self.y + d.y, self.w, self.h)
    }

    #[must_use]
    pub fn corner(self, c: Corner) -> Point {
        match c {
            Corner::TopLeft => Point::new(self.x, self.y),
            Corner::TopRight => Point::new(self.right(), self.y),
            Corner::BottomLeft => Point::new(self.x, self.bottom()),
            Corner::BottomRight => Point::new(self.right(), self.bottom()),
        }
    }

    /// The bounds of `rects`; `None` when empty.
    #[must_use]
    pub fn bounds(rects: impl IntoIterator<Item = Self>) -> Option<Self> {
        rects.into_iter().reduce(Self::union)
    }
}

/// A resize corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Self; 4] = [Self::TopLeft, Self::TopRight, Self::BottomLeft, Self::BottomRight];

    /// The corner across: the one that stays put while this one is dragged.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::TopLeft => Self::BottomRight,
            Self::TopRight => Self::BottomLeft,
            Self::BottomLeft => Self::TopRight,
            Self::BottomRight => Self::TopLeft,
        }
    }

    /// Whether dragging it moves the left / top edge.
    #[must_use]
    pub fn left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft)
    }

    #[must_use]
    pub fn top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight)
    }
}

/// Distance from `p` to the segment `a`–`b`.
#[must_use]
pub fn dist_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 <= f64::EPSILON {
        return p.dist(a);
    }
    let t = (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / len2).clamp(0.0, 1.0);
    p.dist(a + ab * t)
}
