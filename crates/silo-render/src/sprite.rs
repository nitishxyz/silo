//! Procedural, cell-exact geometry for block elements, box drawing, Braille
//! and Powerline code points, in logical pixels and free of GPUI types so it
//! can be regression-tested without a window.
//!
//! Font glyphs for these ranges do not fill the cell's ascent+descent box and
//! may be narrower than the cell advance, so tiling them from the font leaves
//! visible seams. Like Ghostty's sprite font, every shape here is sized to the
//! cell and snapped to whole device pixels so neighbouring cells join exactly.

/// One axis-aligned rectangle; `alpha` scales the cell's foreground alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Quad {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub alpha: f32,
}

/// One convex polygon in paint order; rendered as a triangle fan from the
/// first vertex.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Polygon(pub Vec<(f32, f32)>);

#[derive(Debug, Default, PartialEq)]
pub(super) struct Sprite {
    pub quads: Vec<Quad>,
    pub polygons: Vec<Polygon>,
}

/// The single code point of `text` when the renderer should draw it
/// procedurally instead of shaping it with the font.
pub(super) fn code_point(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    (chars.next().is_none() && is_sprite(first)).then_some(first)
}

pub(super) fn is_sprite(c: char) -> bool {
    matches!(
        c,
        '\u{2500}'..='\u{259F}' | '\u{2800}'..='\u{28FF}' | '\u{E0B0}'..='\u{E0BF}'
    )
}

/// Geometry for `c` drawn into the cell at (`x`, `y`) of the given size.
/// `None` when `c` is not a sprite code point.
pub(super) fn sprite(
    c: char,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    scale: f32,
) -> Option<Sprite> {
    let cell = Cell::new(x, y, width, height, scale);
    let mut out = Sprite::default();
    match c {
        '\u{2500}'..='\u{257F}' => box_drawing(c, &cell, &mut out),
        '\u{2580}'..='\u{259F}' => block(c, &cell, &mut out),
        '\u{2800}'..='\u{28FF}' => braille(c, &cell, &mut out),
        '\u{E0B0}'..='\u{E0BF}' => powerline(c, &cell, &mut out),
        _ => return None,
    }
    Some(out)
}

#[derive(Clone, Copy)]
struct Cell {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    scale: f32,
}

impl Cell {
    fn new(x: f32, y: f32, width: f32, height: f32, scale: f32) -> Self {
        let scale = if scale.is_finite() {
            scale.max(0.5)
        } else {
            1.0
        };
        let device = 1.0 / scale;
        Self {
            x,
            y,
            w: if width.is_finite() {
                width.max(device)
            } else {
                device
            },
            h: if height.is_finite() {
                height.max(device)
            } else {
                device
            },
            scale,
        }
    }

    fn device(&self) -> f32 {
        1.0 / self.scale
    }

    /// Snap a length or offset (relative to the cell origin) to device pixels.
    fn snap(&self, value: f32) -> f32 {
        (value * self.scale).round() / self.scale
    }

    fn right(&self) -> f32 {
        self.x + self.w
    }

    fn bottom(&self) -> f32 {
        self.y + self.h
    }

    fn stroke(&self, fraction: f32) -> f32 {
        self.snap(self.w * fraction)
            .max(self.device())
            .min(self.w)
            .min(self.h)
    }

    fn light(&self) -> f32 {
        self.stroke(1.0 / 8.0)
    }

    fn heavy(&self) -> f32 {
        self.stroke(1.0 / 4.0)
    }

    /// Left edge of a vertical stroke of thickness `t` centred on the cell.
    fn stroke_x(&self, t: f32) -> f32 {
        self.x + self.snap((self.w - t) / 2.0)
    }

    /// Top edge of a horizontal stroke of thickness `t` centred on the cell.
    fn stroke_y(&self, t: f32) -> f32 {
        self.y + self.snap((self.h - t) / 2.0)
    }

    fn mid_x(&self) -> f32 {
        self.x + self.snap(self.w / 2.0)
    }

    fn mid_y(&self) -> f32 {
        self.y + self.snap(self.h / 2.0)
    }
}

fn push_rect(out: &mut Sprite, x0: f32, y0: f32, x1: f32, y1: f32, alpha: f32) {
    if x1 > x0 && y1 > y0 {
        out.quads.push(Quad {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
            alpha,
        });
    }
}

fn rect(out: &mut Sprite, x0: f32, y0: f32, x1: f32, y1: f32) {
    push_rect(out, x0, y0, x1, y1, 1.0);
}

/// A straight stroke of thickness `t` from `p` to `q` as one parallelogram.
fn segment(out: &mut Sprite, p: (f32, f32), q: (f32, f32), t: f32) {
    let (dx, dy) = (q.0 - p.0, q.1 - p.1);
    let len = dx.hypot(dy);
    if len <= 0.0 {
        return;
    }
    let (nx, ny) = (-dy / len * t / 2.0, dx / len * t / 2.0);
    out.polygons.push(Polygon(vec![
        (p.0 + nx, p.1 + ny),
        (q.0 + nx, q.1 + ny),
        (q.0 - nx, q.1 - ny),
        (p.0 - nx, p.1 - ny),
    ]));
}

/// A stroked elliptical arc, centre (`cx`, `cy`), radii (`rx`, `ry`),
/// from `from` to `to` degrees, thickness `t`, as convex quads per step.
fn arc(out: &mut Sprite, (cx, cy): (f32, f32), (rx, ry): (f32, f32), from: f32, to: f32, t: f32) {
    const STEPS: usize = 8;
    let at = |angle: f32, grow: f32| {
        let (sin, cos) = angle.to_radians().sin_cos();
        (cx + (rx + grow) * cos, cy + (ry + grow) * sin)
    };
    for step in 0..STEPS {
        let a0 = from + (to - from) * fraction(step, STEPS);
        let a1 = from + (to - from) * fraction(step + 1, STEPS);
        out.polygons.push(Polygon(vec![
            at(a0, t / 2.0),
            at(a1, t / 2.0),
            at(a1, -t / 2.0),
            at(a0, -t / 2.0),
        ]));
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "step and dash counts are tiny and exactly representable"
)]
fn to_f32(count: usize) -> f32 {
    count as f32
}

fn fraction(step: usize, steps: usize) -> f32 {
    to_f32(step) / to_f32(steps)
}

// ---------------------------------------------------------------------------
// Block elements U+2580..=U+259F

fn block(c: char, cell: &Cell, out: &mut Sprite) {
    let (x, y, right, bottom) = (cell.x, cell.y, cell.right(), cell.bottom());
    let half_w = x + cell.snap(cell.w / 2.0);
    let half_h = y + cell.snap(cell.h / 2.0);
    match c {
        '\u{2580}' => rect(out, x, y, right, half_h),
        '\u{2581}'..='\u{2588}' => {
            let eighths = f32::from(u8::try_from(u32::from(c) - 0x2580).unwrap_or(8));
            let top = bottom - cell.snap(cell.h * eighths / 8.0).max(cell.device());
            rect(out, x, top, right, bottom);
        }
        '\u{2589}'..='\u{258F}' => {
            let eighths = f32::from(u8::try_from(0x2590 - u32::from(c)).unwrap_or(1));
            let width = cell.snap(cell.w * eighths / 8.0).max(cell.device());
            rect(out, x, y, x + width, bottom);
        }
        '\u{2590}' => rect(out, half_w, y, right, bottom),
        '\u{2591}' => push_rect(out, x, y, right, bottom, 0.25),
        '\u{2592}' => push_rect(out, x, y, right, bottom, 0.5),
        '\u{2593}' => push_rect(out, x, y, right, bottom, 0.75),
        '\u{2594}' => rect(
            out,
            x,
            y,
            right,
            y + cell.snap(cell.h / 8.0).max(cell.device()),
        ),
        '\u{2595}' => rect(
            out,
            right - cell.snap(cell.w / 8.0).max(cell.device()),
            y,
            right,
            bottom,
        ),
        '\u{2596}'..='\u{259F}' => {
            // Bits: upper-left 1, upper-right 2, lower-left 4, lower-right 8.
            const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
            let bits = QUADRANTS[(u32::from(c) - 0x2596) as usize];
            for (mask_left, mask_right, top, bot) in [(1, 2, y, half_h), (4, 8, half_h, bottom)] {
                match ((bits & mask_left) != 0, (bits & mask_right) != 0) {
                    (true, true) => rect(out, x, top, right, bot),
                    (true, false) => rect(out, x, top, half_w, bot),
                    (false, true) => rect(out, half_w, top, right, bot),
                    (false, false) => {}
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Box drawing U+2500..=U+257F

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    None,
    Light,
    Heavy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arms {
    up: Weight,
    down: Weight,
    left: Weight,
    right: Weight,
}

const fn arms(up: Weight, down: Weight, left: Weight, right: Weight) -> Arms {
    Arms {
        up,
        down,
        left,
        right,
    }
}

/// Single-stroke (light/heavy) line pieces, indexed by code point.
fn light_heavy_arms(c: char) -> Option<Arms> {
    use Weight::{Heavy as H, Light as L, None as N};
    const CORE: [Arms; 64] = [
        arms(N, L, N, L), // 250C
        arms(N, L, N, H),
        arms(N, H, N, L),
        arms(N, H, N, H),
        arms(N, L, L, N), // 2510
        arms(N, L, H, N),
        arms(N, H, L, N),
        arms(N, H, H, N),
        arms(L, N, N, L), // 2514
        arms(L, N, N, H),
        arms(H, N, N, L),
        arms(H, N, N, H),
        arms(L, N, L, N), // 2518
        arms(L, N, H, N),
        arms(H, N, L, N),
        arms(H, N, H, N),
        arms(L, L, N, L), // 251C
        arms(L, L, N, H),
        arms(H, L, N, L),
        arms(L, H, N, L),
        arms(H, H, N, L), // 2520
        arms(H, L, N, H),
        arms(L, H, N, H),
        arms(H, H, N, H),
        arms(L, L, L, N), // 2524
        arms(L, L, H, N),
        arms(H, L, L, N),
        arms(L, H, L, N),
        arms(H, H, L, N), // 2528
        arms(H, L, H, N),
        arms(L, H, H, N),
        arms(H, H, H, N),
        arms(N, L, L, L), // 252C
        arms(N, L, H, L),
        arms(N, L, L, H),
        arms(N, L, H, H),
        arms(N, H, L, L), // 2530
        arms(N, H, H, L),
        arms(N, H, L, H),
        arms(N, H, H, H),
        arms(L, N, L, L), // 2534
        arms(L, N, H, L),
        arms(L, N, L, H),
        arms(L, N, H, H),
        arms(H, N, L, L), // 2538
        arms(H, N, H, L),
        arms(H, N, L, H),
        arms(H, N, H, H),
        arms(L, L, L, L), // 253C
        arms(L, L, H, L),
        arms(L, L, L, H),
        arms(L, L, H, H),
        arms(H, L, L, L), // 2540
        arms(L, H, L, L),
        arms(H, H, L, L),
        arms(H, L, H, L),
        arms(H, L, L, H), // 2544
        arms(L, H, H, L),
        arms(L, H, L, H),
        arms(H, L, H, H),
        arms(L, H, H, H), // 2548
        arms(H, H, H, L),
        arms(H, H, L, H),
        arms(H, H, H, H),
    ];
    const HALVES: [Arms; 12] = [
        arms(N, N, L, N), // 2574
        arms(L, N, N, N),
        arms(N, N, N, L),
        arms(N, L, N, N),
        arms(N, N, H, N), // 2578
        arms(H, N, N, N),
        arms(N, N, N, H),
        arms(N, H, N, N),
        arms(N, N, L, H), // 257C
        arms(L, H, N, N),
        arms(N, N, H, L),
        arms(H, L, N, N),
    ];
    let code = u32::from(c);
    Some(match code {
        0x2500 => arms(N, N, L, L),
        0x2501 => arms(N, N, H, H),
        0x2502 => arms(L, L, N, N),
        0x2503 => arms(H, H, N, N),
        0x250C..=0x254B => CORE[(code - 0x250C) as usize],
        0x2574..=0x257F => HALVES[(code - 0x2574) as usize],
        _ => return None,
    })
}

fn thickness(cell: &Cell, weight: Weight) -> f32 {
    match weight {
        Weight::None => 0.0,
        Weight::Light => cell.light(),
        Weight::Heavy => cell.heavy(),
    }
}

/// Arms run from the cell edge to a shared junction box; the junction is one
/// separate rectangle so nothing overlaps (overlaps would double faint alpha).
fn draw_arms(cell: &Cell, arms: Arms, out: &mut Sprite) {
    let Arms {
        up,
        down,
        left,
        right,
    } = arms;
    let vertical = [up, down]
        .into_iter()
        .filter(|w| *w != Weight::None)
        .map(|w| {
            let t = thickness(cell, w);
            let x0 = cell.stroke_x(t);
            (x0, x0 + t)
        })
        .reduce(|acc, r| (acc.0.min(r.0), acc.1.max(r.1)));
    let horizontal = [left, right]
        .into_iter()
        .filter(|w| *w != Weight::None)
        .map(|w| {
            let t = thickness(cell, w);
            let y0 = cell.stroke_y(t);
            (y0, y0 + t)
        })
        .reduce(|acc, r| (acc.0.min(r.0), acc.1.max(r.1)));
    let (up_end, down_start) = horizontal.unwrap_or((cell.mid_y(), cell.mid_y()));
    let (left_end, right_start) = vertical.unwrap_or((cell.mid_x(), cell.mid_x()));
    if up != Weight::None && up == down && horizontal.is_none() {
        let t = thickness(cell, up);
        let x0 = cell.stroke_x(t);
        rect(out, x0, cell.y, x0 + t, cell.bottom());
    } else {
        if up != Weight::None {
            let t = thickness(cell, up);
            let x0 = cell.stroke_x(t);
            rect(out, x0, cell.y, x0 + t, up_end);
        }
        if down != Weight::None {
            let t = thickness(cell, down);
            let x0 = cell.stroke_x(t);
            rect(out, x0, down_start, x0 + t, cell.bottom());
        }
    }
    if left != Weight::None && left == right && vertical.is_none() {
        let t = thickness(cell, left);
        let y0 = cell.stroke_y(t);
        rect(out, cell.x, y0, cell.right(), y0 + t);
    } else {
        if left != Weight::None {
            let t = thickness(cell, left);
            let y0 = cell.stroke_y(t);
            rect(out, cell.x, y0, left_end, y0 + t);
        }
        if right != Weight::None {
            let t = thickness(cell, right);
            let y0 = cell.stroke_y(t);
            rect(out, right_start, y0, cell.right(), y0 + t);
        }
    }
    if let (Some((x0, x1)), Some((y0, y1))) = (vertical, horizontal) {
        rect(out, x0, y0, x1, y1);
    }
}

/// `count` dashes along one axis, with a gap centred on each cell edge so a
/// run of dashed cells continues the pattern.
fn dashes(cell: &Cell, count: usize, vertical: bool, t: f32, out: &mut Sprite) {
    let extent = if vertical { cell.h } else { cell.w };
    let period = extent / to_f32(count.max(1));
    let gap = cell.snap(period / 3.0).max(cell.device());
    let half_gap = cell.snap(gap / 2.0);
    for index in 0..count {
        let start = cell.snap(period * to_f32(index) + half_gap);
        let end = cell.snap(period * to_f32(index + 1) - (gap - half_gap));
        if vertical {
            let x0 = cell.stroke_x(t);
            rect(out, x0, cell.y + start, x0 + t, cell.y + end);
        } else {
            let y0 = cell.stroke_y(t);
            rect(out, cell.x + start, y0, cell.x + end, y0 + t);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Single,
    Double,
}

#[derive(Clone, Copy)]
struct Doubles {
    up: Option<Kind>,
    down: Option<Kind>,
    left: Option<Kind>,
    right: Option<Kind>,
}

fn double_arms(c: char) -> Option<Doubles> {
    use Kind::{Double as D, Single as S};
    let d = |up, down, left, right| Doubles {
        up,
        down,
        left,
        right,
    };
    #[allow(
        clippy::match_same_arms,
        reason = "one row per code point keeps the table auditable"
    )]
    Some(match c {
        '\u{2550}' => d(None, None, Some(D), Some(D)),
        '\u{2551}' => d(Some(D), Some(D), None, None),
        '\u{2552}' => d(None, Some(S), None, Some(D)),
        '\u{2553}' => d(None, Some(D), None, Some(S)),
        '\u{2554}' => d(None, Some(D), None, Some(D)),
        '\u{2555}' => d(None, Some(S), Some(D), None),
        '\u{2556}' => d(None, Some(D), Some(S), None),
        '\u{2557}' => d(None, Some(D), Some(D), None),
        '\u{2558}' => d(Some(S), None, None, Some(D)),
        '\u{2559}' => d(Some(D), None, None, Some(S)),
        '\u{255A}' => d(Some(D), None, None, Some(D)),
        '\u{255B}' => d(Some(S), None, Some(D), None),
        '\u{255C}' => d(Some(D), None, Some(S), None),
        '\u{255D}' => d(Some(D), None, Some(D), None),
        '\u{255E}' => d(Some(S), Some(S), None, Some(D)),
        '\u{255F}' => d(Some(D), Some(D), None, Some(S)),
        '\u{2560}' => d(Some(D), Some(D), None, Some(D)),
        '\u{2561}' => d(Some(S), Some(S), Some(D), None),
        '\u{2562}' => d(Some(D), Some(D), Some(S), None),
        '\u{2563}' => d(Some(D), Some(D), Some(D), None),
        '\u{2564}' => d(None, Some(S), Some(D), Some(D)),
        '\u{2565}' => d(None, Some(D), Some(S), Some(S)),
        '\u{2566}' => d(None, Some(D), Some(D), Some(D)),
        '\u{2567}' => d(Some(S), None, Some(D), Some(D)),
        '\u{2568}' => d(Some(D), None, Some(S), Some(S)),
        '\u{2569}' => d(Some(D), None, Some(D), Some(D)),
        '\u{256A}' => d(Some(S), Some(S), Some(D), Some(D)),
        '\u{256B}' => d(Some(D), Some(D), Some(S), Some(S)),
        '\u{256C}' => d(Some(D), Some(D), Some(D), Some(D)),
        _ => return None,
    })
}

/// Double-line pieces. Vertical strokes own the junction corners and
/// Double-line pieces. Vertical strokes own the junction corners and
/// horizontal strokes stop at vertical stroke edges, so nothing overlaps and
/// double corners keep their open inner corner.
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive junction table is clearer than scattered helpers"
)]
fn draw_doubles(cell: &Cell, pieces: Doubles, out: &mut Sprite) {
    let t = cell.light();
    let (x, y, right, bottom) = (cell.x, cell.y, cell.right(), cell.bottom());
    let xs = cell.stroke_x(t);
    let (xl, xr) = (xs - t, xs + t);
    let ys = cell.stroke_y(t);
    let (yt, yb) = (ys - t, ys + t);
    let vkind = pieces.up.or(pieces.down).unwrap_or(Kind::Single);
    let hkind = pieces.left.or(pieces.right).unwrap_or(Kind::Single);
    let both_double = vkind == Kind::Double && hkind == Kind::Double;
    let vstrokes: Vec<f32> = match vkind {
        Kind::Single => vec![xs],
        Kind::Double => vec![xl, xr],
    };
    let hstrokes: Vec<f32> = match hkind {
        Kind::Single => vec![ys],
        Kind::Double => vec![yt, yb],
    };
    let last_v = vstrokes.len() - 1;
    let last_h = hstrokes.len() - 1;
    let vert = |out: &mut Sprite, lx: f32, y0: f32, y1: f32| rect(out, lx, y0, lx + t, y1);
    let horiz = |out: &mut Sprite, ly: f32, x0: f32, x1: f32| rect(out, x0, ly, x1, ly + t);

    let has = (
        pieces.up.is_some(),
        pieces.down.is_some(),
        pieces.left.is_some(),
        pieces.right.is_some(),
    );
    match has {
        (false, false, false, false) => {}
        (true, true, true, true) => match (vkind, hkind) {
            (Kind::Single, Kind::Double) => {
                vert(out, xs, y, bottom);
                for ly in hstrokes {
                    horiz(out, ly, x, xs);
                    horiz(out, ly, xs + t, right);
                }
            }
            (Kind::Double, Kind::Single) => {
                vert(out, xl, y, bottom);
                vert(out, xr, y, bottom);
                horiz(out, ys, x, xl);
                horiz(out, ys, xl + t, xr);
                horiz(out, ys, xr + t, right);
            }
            _ => {
                for lx in vstrokes {
                    vert(out, lx, y, yt + t);
                    vert(out, lx, yb, bottom);
                }
                for ly in hstrokes {
                    horiz(out, ly, x, xl);
                    horiz(out, ly, xr + t, right);
                }
            }
        },
        (up, down, false, false) => {
            for lx in vstrokes {
                let y0 = if up { y } else { ys };
                let y1 = if down { bottom } else { ys + t };
                vert(out, lx, y0, y1);
            }
        }
        (false, false, left, right_arm) => {
            for ly in hstrokes {
                let x0 = if left { x } else { xs };
                let x1 = if right_arm { right } else { xs + t };
                horiz(out, ly, x0, x1);
            }
        }
        (true, true, left, _) => {
            // Vertical passes through; one horizontal arm meets it at the
            // nearest vertical stroke.
            let near = if left { 0 } else { last_v };
            for (index, lx) in vstrokes.iter().enumerate() {
                if index == near && both_double {
                    vert(out, *lx, y, yt + t);
                    vert(out, *lx, yb, bottom);
                } else {
                    vert(out, *lx, y, bottom);
                }
            }
            for ly in hstrokes {
                if left {
                    horiz(out, ly, x, vstrokes[near]);
                } else {
                    horiz(out, ly, vstrokes[near] + t, right);
                }
            }
        }
        (up, _, true, true) => {
            // Horizontal passes through; one vertical arm meets it at the
            // nearest horizontal stroke.
            let near = if up { 0 } else { last_h };
            for (index, ly) in hstrokes.iter().enumerate() {
                if index == near && both_double {
                    horiz(out, *ly, x, xl);
                    horiz(out, *ly, xr + t, right);
                } else {
                    horiz(out, *ly, x, right);
                }
            }
            let ly = hstrokes[near];
            for lx in vstrokes {
                match (up, both_double) {
                    (true, true) => vert(out, lx, y, ly + t),
                    (true, false) => vert(out, lx, y, ly),
                    (false, true) => vert(out, lx, ly, bottom),
                    (false, false) => vert(out, lx, ly + t, bottom),
                }
            }
        }
        (up, _, left, _) => {
            // Corner: one vertical arm and one horizontal arm. Outer strokes
            // lie opposite the arm directions and meet each other.
            let outer_v = if left { last_v } else { 0 };
            let outer_h = if up { last_h } else { 0 };
            let vert_to = |out: &mut Sprite, lx: f32, ly: f32, own: bool| match (up, own) {
                (true, true) => vert(out, lx, y, ly + t),
                (true, false) => vert(out, lx, y, ly),
                (false, true) => vert(out, lx, ly, bottom),
                (false, false) => vert(out, lx, ly + t, bottom),
            };
            let horiz_from = |out: &mut Sprite, ly: f32, lx: f32| {
                if left {
                    horiz(out, ly, x, lx);
                } else {
                    horiz(out, ly, lx + t, right);
                }
            };
            match (vkind, hkind) {
                (Kind::Double, Kind::Double) => {
                    for (index, lx) in vstrokes.iter().enumerate() {
                        let ly = if index == outer_v {
                            hstrokes[outer_h]
                        } else {
                            hstrokes[1 - outer_h]
                        };
                        vert_to(out, *lx, ly, true);
                        horiz_from(out, ly, *lx);
                    }
                }
                (Kind::Single, _) => {
                    vert_to(out, xs, hstrokes[outer_h], true);
                    for ly in hstrokes {
                        horiz_from(out, ly, xs);
                    }
                }
                (Kind::Double, Kind::Single) => {
                    for (index, lx) in vstrokes.iter().enumerate() {
                        vert_to(out, *lx, ys, index == outer_v);
                    }
                    horiz_from(out, ys, vstrokes[outer_v]);
                }
            }
        }
    }
}

fn box_drawing(c: char, cell: &Cell, out: &mut Sprite) {
    if let Some(arms) = light_heavy_arms(c) {
        draw_arms(cell, arms, out);
        return;
    }
    if let Some(pieces) = double_arms(c) {
        draw_doubles(cell, pieces, out);
        return;
    }
    let (light, heavy) = (cell.light(), cell.heavy());
    match c {
        '\u{2504}' => dashes(cell, 3, false, light, out),
        '\u{2505}' => dashes(cell, 3, false, heavy, out),
        '\u{2506}' => dashes(cell, 3, true, light, out),
        '\u{2507}' => dashes(cell, 3, true, heavy, out),
        '\u{2508}' => dashes(cell, 4, false, light, out),
        '\u{2509}' => dashes(cell, 4, false, heavy, out),
        '\u{250A}' => dashes(cell, 4, true, light, out),
        '\u{250B}' => dashes(cell, 4, true, heavy, out),
        '\u{254C}' => dashes(cell, 2, false, light, out),
        '\u{254D}' => dashes(cell, 2, false, heavy, out),
        '\u{254E}' => dashes(cell, 2, true, light, out),
        '\u{254F}' => dashes(cell, 2, true, heavy, out),
        '\u{256D}'..='\u{2570}' => {
            let (down, right_arm) = match c {
                '\u{256D}' => (true, true),
                '\u{256E}' => (true, false),
                '\u{256F}' => (false, false),
                _ => (false, true),
            };
            let (xs, ys) = (cell.stroke_x(light), cell.stroke_y(light));
            let (cx, cy) = (xs + light / 2.0, ys + light / 2.0);
            // The arc's outer edge ends exactly on the cell edge, so a
            // neighbouring straight line continues it without a gap.
            let r = ((cell.w.min(cell.h) - light) / 2.0).max(light);
            if down {
                rect(out, xs, cy + r, xs + light, cell.bottom());
            } else {
                rect(out, xs, cell.y, xs + light, cy - r);
            }
            if right_arm {
                rect(out, cx + r, ys, cell.right(), ys + light);
            } else {
                rect(out, cell.x, ys, cx - r, ys + light);
            }
            let centre = (
                if right_arm { cx + r } else { cx - r },
                if down { cy + r } else { cy - r },
            );
            let (from, to) = match (down, right_arm) {
                (true, true) => (180.0, 270.0),
                (true, false) => (0.0, -90.0),
                (false, false) => (0.0, 90.0),
                (false, true) => (180.0, 90.0),
            };
            arc(out, centre, (r, r), from, to, light);
        }
        '\u{2571}' | '\u{2572}' | '\u{2573}' => {
            if c != '\u{2572}' {
                segment(out, (cell.x, cell.bottom()), (cell.right(), cell.y), light);
            }
            if c != '\u{2571}' {
                segment(out, (cell.x, cell.y), (cell.right(), cell.bottom()), light);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Braille U+2800..=U+28FF: a 2x4 dot grid.

fn braille(c: char, cell: &Cell, out: &mut Sprite) {
    let bits = u32::from(c) - 0x2800;
    let dot = cell
        .snap((cell.w / 4.0).min(cell.h / 8.0))
        .max(cell.device());
    for (bit, column, row) in [
        (0x01_u32, 0_u8, 0_u8),
        (0x02, 0, 1),
        (0x04, 0, 2),
        (0x08, 1, 0),
        (0x10, 1, 1),
        (0x20, 1, 2),
        (0x40, 0, 3),
        (0x80, 1, 3),
    ] {
        if (bits & bit) == 0 {
            continue;
        }
        let centre_x = cell.w * (2.0 * f32::from(column) + 1.0) / 4.0;
        let centre_y = cell.h * (2.0 * f32::from(row) + 1.0) / 8.0;
        let x0 = cell.x + cell.snap(centre_x - dot / 2.0);
        let y0 = cell.y + cell.snap(centre_y - dot / 2.0);
        rect(out, x0, y0, x0 + dot, y0 + dot);
    }
}

// ---------------------------------------------------------------------------
// Powerline private-use U+E0B0..=U+E0BF

fn powerline(c: char, cell: &Cell, out: &mut Sprite) {
    let (x, y, right, bottom) = (cell.x, cell.y, cell.right(), cell.bottom());
    let cy = cell.y + cell.h / 2.0;
    let t = cell.light();
    let half = |out: &mut Sprite, centre_x: f32, sign: f32, from: f32, to: f32| {
        // Solid half ellipse spanning the full cell height and width.
        const STEPS: usize = 12;
        let points = (0..=STEPS)
            .map(|step| {
                let angle = (from + (to - from) * fraction(step, STEPS)).to_radians();
                (
                    centre_x + sign * cell.w * angle.cos(),
                    cy + cell.h / 2.0 * angle.sin(),
                )
            })
            .collect();
        out.polygons.push(Polygon(points));
    };
    match c {
        '\u{E0B0}' => out
            .polygons
            .push(Polygon(vec![(x, y), (right, cy), (x, bottom)])),
        '\u{E0B1}' => {
            segment(out, (x, y), (right, cy), t);
            segment(out, (right, cy), (x, bottom), t);
        }
        '\u{E0B2}' => out
            .polygons
            .push(Polygon(vec![(right, y), (x, cy), (right, bottom)])),
        '\u{E0B3}' => {
            segment(out, (right, y), (x, cy), t);
            segment(out, (x, cy), (right, bottom), t);
        }
        '\u{E0B4}' => half(out, x, 1.0, -90.0, 90.0),
        '\u{E0B5}' => arc(out, (x, cy), (cell.w, cell.h / 2.0), -90.0, 90.0, t),
        '\u{E0B6}' => half(out, right, -1.0, -90.0, 90.0),
        '\u{E0B7}' => arc(out, (right, cy), (cell.w, cell.h / 2.0), 90.0, 270.0, t),
        '\u{E0B8}' => out
            .polygons
            .push(Polygon(vec![(x, y), (right, bottom), (x, bottom)])),
        '\u{E0B9}' | '\u{E0BF}' => segment(out, (x, y), (right, bottom), t),
        '\u{E0BA}' => out
            .polygons
            .push(Polygon(vec![(right, y), (right, bottom), (x, bottom)])),
        '\u{E0BB}' | '\u{E0BD}' => segment(out, (right, y), (x, bottom), t),
        '\u{E0BC}' => out
            .polygons
            .push(Polygon(vec![(x, y), (right, y), (x, bottom)])),
        '\u{E0BE}' => out
            .polygons
            .push(Polygon(vec![(x, y), (right, y), (right, bottom)])),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Geometry cache

/// Sprite geometry is translation invariant: every coordinate is the cell
/// origin plus a length derived from the cell size and scale alone, and the
/// device-pixel snapping is applied to those lengths, not to absolute
/// positions. One entry per (code point, cell width) therefore serves every
/// cell of the grid; the table is dropped whole when the cell height or the
/// display scale changes.
///
/// Braille-heavy TUIs (graph plots) otherwise rebuild the same handful of
/// shapes for thousands of cells on every frame.
#[derive(Default)]
pub(crate) struct SpriteCache {
    /// Cell height and scale the entries were built for.
    key: (u32, u32),
    entries: std::collections::HashMap<(char, u32), Option<Sprite>>,
}

impl SpriteCache {
    /// Geometry for `c` in a cell of this size, with the cell origin at
    /// (0, 0); `None` when `c` is not a sprite code point.
    pub(crate) fn geometry(
        &mut self,
        c: char,
        width: f32,
        height: f32,
        scale: f32,
    ) -> Option<&Sprite> {
        let key = (height.to_bits(), scale.to_bits());
        if key != self.key {
            self.entries.clear();
            self.key = key;
        }
        self.entries
            .entry((c, width.to_bits()))
            .or_insert_with(|| sprite(c, 0.0, 0.0, width, height, scale))
            .as_ref()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Quad, Sprite, SpriteCache, code_point, is_sprite, sprite};

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn quad(x: f32, y: f32, width: f32, height: f32) -> Quad {
        Quad {
            x,
            y,
            width,
            height,
            alpha: 1.0,
        }
    }

    fn same(actual: &[Quad], expected: &[Quad]) -> bool {
        actual.len() == expected.len()
            && actual.iter().zip(expected).all(|(a, e)| {
                near(a.x, e.x)
                    && near(a.y, e.y)
                    && near(a.width, e.width)
                    && near(a.height, e.height)
                    && near(a.alpha, e.alpha)
            })
    }

    fn quads(c: char, x: f32, y: f32) -> Vec<Quad> {
        sprite(c, x, y, 8.0, 20.0, 1.0).expect("sprite").quads
    }

    fn within(sprite: &Sprite, x: f32, y: f32, w: f32, h: f32) -> bool {
        sprite.quads.iter().all(|q| {
            q.x >= x - 1e-4
                && q.y >= y - 1e-4
                && q.x + q.width <= x + w + 1e-4
                && q.y + q.height <= y + h + 1e-4
        })
    }

    #[test]
    fn classifies_sprite_ranges_and_single_code_points_only() {
        assert_eq!(code_point("█"), Some('█'));
        assert_eq!(code_point("─"), Some('─'));
        assert_eq!(code_point("╬"), Some('╬'));
        assert_eq!(code_point("\u{E0B0}"), Some('\u{E0B0}'));
        assert_eq!(code_point("⣿"), Some('⣿'));
        assert_eq!(code_point("A"), None);
        assert_eq!(code_point(""), None);
        assert_eq!(
            code_point("█\u{0301}"),
            None,
            "combining sequences use the font"
        );
        assert!(!is_sprite('\u{24FF}'));
        assert!(!is_sprite('\u{25A0}'));
        assert!(!is_sprite('\u{E0C0}'));
        assert!(sprite('A', 0.0, 0.0, 8.0, 20.0, 1.0).is_none());
    }

    #[test]
    fn full_and_half_blocks_fill_exact_cell_fractions() {
        assert!(same(
            &quads('█', 16.0, 40.0),
            &[quad(16.0, 40.0, 8.0, 20.0)]
        ));
        assert!(same(&quads('▀', 0.0, 0.0), &[quad(0.0, 0.0, 8.0, 10.0)]));
        assert!(same(&quads('▄', 0.0, 0.0), &[quad(0.0, 10.0, 8.0, 10.0)]));
        assert!(same(&quads('▌', 0.0, 0.0), &[quad(0.0, 0.0, 4.0, 20.0)]));
        assert!(same(&quads('▐', 0.0, 0.0), &[quad(4.0, 0.0, 4.0, 20.0)]));
        // 20 / 8 = 2.5 device pixels rounds to 3.
        assert!(same(&quads('▁', 0.0, 0.0), &[quad(0.0, 17.0, 8.0, 3.0)]));
        assert!(same(&quads('▏', 0.0, 0.0), &[quad(0.0, 0.0, 1.0, 20.0)]));
        let shade = quads('▒', 0.0, 0.0);
        assert!(same(
            &shade,
            &[Quad {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 20.0,
                alpha: 0.5
            }]
        ));
        assert!(near(quads('░', 0.0, 0.0)[0].alpha, 0.25));
        assert!(near(quads('▓', 0.0, 0.0)[0].alpha, 0.75));
        // ▚ = upper-left + lower-right, ▛ merges the full upper row.
        assert!(same(
            &quads('▚', 0.0, 0.0),
            &[quad(0.0, 0.0, 4.0, 10.0), quad(4.0, 10.0, 4.0, 10.0)]
        ));
        assert!(same(
            &quads('▛', 0.0, 0.0),
            &[quad(0.0, 0.0, 8.0, 10.0), quad(0.0, 10.0, 4.0, 10.0)]
        ));
    }

    #[test]
    fn light_lines_are_one_device_pixel_on_the_cell_midline() {
        // 8 px wide: light = 1 px at x = 4 (round(3.5)), y = round(9.5) = 10.
        assert!(same(&quads('─', 0.0, 0.0), &[quad(0.0, 10.0, 8.0, 1.0)]));
        assert!(same(&quads('│', 0.0, 0.0), &[quad(4.0, 0.0, 1.0, 20.0)]));
        // Heavy = 2 px, centred: x = 3, y = 9.
        assert!(same(&quads('━', 0.0, 0.0), &[quad(0.0, 9.0, 8.0, 2.0)]));
        assert!(same(&quads('┃', 0.0, 0.0), &[quad(3.0, 0.0, 2.0, 20.0)]));
    }

    #[test]
    fn corners_join_arms_through_a_single_junction_square() {
        // ┌: down arm below the junction, right arm right of it, junction square.
        let corner = quads('┌', 0.0, 0.0);
        assert!(same(
            &corner,
            &[
                quad(4.0, 11.0, 1.0, 9.0),
                quad(5.0, 10.0, 3.0, 1.0),
                quad(4.0, 10.0, 1.0, 1.0),
            ]
        ));
        let cross = quads('┼', 0.0, 0.0);
        assert_eq!(cross.len(), 5);
        assert!(
            cross
                .iter()
                .any(|q| near(q.x, 4.0) && near(q.y, 10.0) && near(q.width, 1.0))
        );
        // Every piece stays inside the cell.
        for c in [
            '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼', '╋', '╬', '╭', '╯',
        ] {
            let s = sprite(c, 3.0, 5.0, 8.0, 20.0, 1.0).expect("sprite");
            assert!(within(&s, 3.0, 5.0, 8.0, 20.0), "{c}: {s:?}");
        }
    }

    #[test]
    fn double_lines_are_two_light_strokes_with_open_corners() {
        assert!(same(
            &quads('═', 0.0, 0.0),
            &[quad(0.0, 9.0, 8.0, 1.0), quad(0.0, 11.0, 8.0, 1.0)]
        ));
        assert!(same(
            &quads('║', 0.0, 0.0),
            &[quad(3.0, 0.0, 1.0, 20.0), quad(5.0, 0.0, 1.0, 20.0)]
        ));
        // ╔: outer strokes (x=3, y=9) meet, inner strokes (x=5, y=11) meet.
        let corner = quads('╔', 0.0, 0.0);
        assert!(same(
            &corner,
            &[
                quad(3.0, 9.0, 1.0, 11.0),
                quad(4.0, 9.0, 4.0, 1.0),
                quad(5.0, 11.0, 1.0, 9.0),
                quad(6.0, 11.0, 2.0, 1.0),
            ]
        ));
        // No quad overlaps another (faint alpha would otherwise double up).
        for c in [
            '╔', '╗', '╚', '╝', '╠', '╣', '╦', '╩', '╬', '╪', '╫', '╒', '╓', '╕', '╖', '╘', '╙',
            '╛', '╜', '╞', '╟', '╡', '╢', '╤', '╥', '╧', '╨',
        ] {
            let pieces = quads(c, 0.0, 0.0);
            assert!(!pieces.is_empty(), "{c}");
            assert!(
                within(
                    &Sprite {
                        quads: pieces.clone(),
                        polygons: Vec::new()
                    },
                    0.0,
                    0.0,
                    8.0,
                    20.0
                ),
                "{c}: {pieces:?}"
            );
            for (index, first) in pieces.iter().enumerate() {
                for second in &pieces[index + 1..] {
                    let overlap = first.x < second.x + second.width - 1e-4
                        && second.x < first.x + first.width - 1e-4
                        && first.y < second.y + second.height - 1e-4
                        && second.y < first.y + first.height - 1e-4;
                    assert!(!overlap, "{c}: {first:?} overlaps {second:?}");
                }
            }
        }
    }

    #[test]
    fn adjacent_cells_touch_exactly() {
        let left = quads('─', 0.0, 0.0);
        let right = quads('─', 8.0, 0.0);
        assert!(near(left[0].x + left[0].width, right[0].x));
        assert!(near(left[0].y, right[0].y));
        let upper = quads('│', 0.0, 0.0);
        let lower = quads('│', 0.0, 20.0);
        assert!(near(upper[0].y + upper[0].height, lower[0].y));
        assert!(near(upper[0].x, lower[0].x));
        let top = quads('█', 0.0, 0.0);
        let below = quads('▀', 0.0, 20.0);
        assert!(near(top[0].y + top[0].height, below[0].y));
        // A corner's arm reaches the neighbouring line's stroke.
        let corner = quads('┌', 0.0, 0.0);
        let line = quads('─', 8.0, 0.0);
        let arm = corner
            .iter()
            .find(|q| near(q.y, 10.0) && near(q.width, 3.0))
            .expect("right arm");
        assert!(near(arm.x + arm.width, line[0].x));
        let down = quads('│', 0.0, 20.0);
        let stem = corner
            .iter()
            .find(|q| near(q.height, 9.0))
            .expect("down arm");
        assert!(near(stem.y + stem.height, down[0].y));
        assert!(near(stem.x, down[0].x));
    }

    #[test]
    fn fractional_scale_snaps_to_device_pixels_without_seams() {
        let scale = 1.25;
        let (width, height) = (8.8, 20.0); // 11 x 25 device pixels
        let lower = sprite('▄', 0.0, 0.0, width, height, scale)
            .expect("sprite")
            .quads;
        let upper = sprite('▀', 0.0, height, width, height, scale)
            .expect("sprite")
            .quads;
        assert!(near(lower[0].y + lower[0].height, upper[0].y));
        assert!(near((lower[0].y * scale).fract(), 0.0), "{}", lower[0].y);
        let line = sprite('─', 8.8, 0.0, width, height, scale)
            .expect("sprite")
            .quads;
        let prev = sprite('─', 0.0, 0.0, width, height, scale)
            .expect("sprite")
            .quads;
        assert!(near(prev[0].x + prev[0].width, line[0].x));
        assert!(near(
            line[0].height * scale,
            (line[0].height * scale).round()
        ));
        assert!(line[0].height * scale >= 1.0);
        assert!(near((line[0].y * scale).fract(), 0.0), "{}", line[0].y);
        let heavy = sprite('┃', 0.0, 0.0, width, height, scale)
            .expect("sprite")
            .quads;
        assert!(near(heavy[0].width * scale, 3.0));
        let tiny = sprite('│', 0.0, 0.0, 3.0, 6.0, 2.0).expect("sprite").quads;
        assert!(near(tiny[0].width, 0.5), "min one device pixel");
    }

    #[test]
    fn powerline_and_diagonals_use_polygons_and_braille_uses_dots() {
        let arrow = sprite('\u{E0B0}', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        let expected = [(0.0, 0.0), (8.0, 10.0), (0.0, 20.0)];
        assert!(
            arrow.polygons[0]
                .0
                .iter()
                .zip(expected)
                .all(|(point, want)| near(point.0, want.0) && near(point.1, want.1))
        );
        assert!(arrow.quads.is_empty());
        let thin = sprite('\u{E0B1}', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(thin.polygons.len(), 2);
        let round = sprite('\u{E0B4}', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(round.polygons.len(), 1);
        assert!(near(round.polygons[0].0[0].0, 0.0) && near(round.polygons[0].0[0].1, 0.0));
        let diagonal = sprite('╱', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(diagonal.polygons.len(), 1);
        assert_eq!(
            sprite('╳', 0.0, 0.0, 8.0, 20.0, 1.0)
                .expect("sprite")
                .polygons
                .len(),
            2
        );
        let arc = sprite('╭', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(arc.polygons.len(), 8);
        assert!(within(&arc, 0.0, 0.0, 8.0, 20.0));
        // The arc's horizontal end lands on the cell edge at the light line's y.
        let end = arc.polygons[7].0[1];
        assert!(near(end.0, 8.0) && near(end.1, 10.0), "{end:?}");
        assert!(
            sprite('\u{2800}', 0.0, 0.0, 8.0, 20.0, 1.0)
                .expect("sprite")
                .quads
                .is_empty()
        );
        let full = sprite('⣿', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(full.quads.len(), 8);
        assert!(within(&full, 0.0, 0.0, 8.0, 20.0));
        let dashed = sprite('┄', 0.0, 0.0, 8.0, 20.0, 1.0).expect("sprite");
        assert_eq!(dashed.quads.len(), 3);
    }

    #[test]
    fn cached_geometry_is_the_cell_shape_translated_to_its_origin() {
        let mut cache = SpriteCache::default();
        for c in ['⣿', '⢰', '█', '▒', '┼', '╭', '\u{E0B0}'] {
            let direct = sprite(c, 24.0, 60.0, 8.0, 20.0, 2.0).expect("sprite");
            let cached = cache.geometry(c, 8.0, 20.0, 2.0).expect("cached sprite");
            let moved: Vec<Quad> = cached
                .quads
                .iter()
                .map(|q| Quad {
                    x: q.x + 24.0,
                    y: q.y + 60.0,
                    ..*q
                })
                .collect();
            assert!(same(&moved, &direct.quads), "{c} quads");
            assert_eq!(cached.polygons.len(), direct.polygons.len(), "{c} polygons");
            for (cached, direct) in cached.polygons.iter().zip(&direct.polygons) {
                assert!(
                    cached.0.iter().zip(&direct.0).all(|(cached, direct)| near(
                        cached.0 + 24.0,
                        direct.0
                    ) && near(
                        cached.1 + 60.0,
                        direct.1
                    )),
                    "{c} polygon vertices"
                );
            }
        }
        assert_eq!(cache.len(), 7, "one entry per code point, not per cell");
        // A different cell height (font size or display change) invalidates
        // every entry; a wide cell coexists with the narrow one.
        let _ = cache.geometry('█', 16.0, 20.0, 2.0);
        assert_eq!(cache.len(), 8);
        let _ = cache.geometry('█', 8.0, 22.0, 2.0);
        assert_eq!(cache.len(), 1);
        assert!(cache.geometry('A', 8.0, 22.0, 2.0).is_none());
    }
}
