//! Dependency-free cursor rectangles in logical pixels, so the geometry can
//! be regression-tested without a GPU window.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Bar,
    Block,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// One logical pixel snapped to whole device pixels: 1 px at 1x, 2 px at 2x.
pub(super) fn stroke(scale: f32) -> f32 {
    let scale = if scale.is_finite() {
        scale.max(0.5)
    } else {
        1.0
    };
    scale.round().max(1.0) / scale
}

/// Rectangles for a cursor occupying the cell at (`x`, `y`) with the given
/// cell size. Every shape stays inside that one cell: the block is exactly
/// the glyph cell, the bar hugs its left edge, the underline its bottom edge.
pub(super) fn rectangles(
    shape: Shape,
    focused: bool,
    x: f32,
    y: f32,
    cell_width: f32,
    cell_height: f32,
    scale: f32,
) -> Vec<Rect> {
    let w = cell_width.max(1.0);
    let h = cell_height.max(1.0);
    let stroke = stroke(scale).min(w).min(h);
    let shape = if focused { shape } else { Shape::Hollow };
    match shape {
        Shape::Bar => vec![Rect {
            x,
            y,
            width: stroke,
            height: h,
        }],
        Shape::Underline => vec![Rect {
            x,
            y: y + h - stroke,
            width: w,
            height: stroke,
        }],
        Shape::Block => vec![Rect {
            x,
            y,
            width: w,
            height: h,
        }],
        Shape::Hollow => vec![
            Rect {
                x,
                y,
                width: w,
                height: stroke,
            },
            Rect {
                x,
                y: y + h - stroke,
                width: w,
                height: stroke,
            },
            Rect {
                x,
                y,
                width: stroke,
                height: h,
            },
            Rect {
                x: x + w - stroke,
                y,
                width: stroke,
                height: h,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{Rect, Shape, rectangles, stroke};

    fn near(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-5
    }

    fn within(rects: &[Rect], x: f32, y: f32, w: f32, h: f32) -> bool {
        rects.iter().all(|rect| {
            rect.x >= x - 1e-4
                && rect.y >= y - 1e-4
                && rect.x + rect.width <= x + w + 1e-4
                && rect.y + rect.height <= y + h + 1e-4
        })
    }

    #[test]
    fn every_shape_stays_inside_the_glyph_cell() {
        let (x, y, w, h) = (40.0, 60.0, 8.0, 20.0);
        for shape in [Shape::Bar, Shape::Block, Shape::Underline, Shape::Hollow] {
            for focused in [true, false] {
                let rects = rectangles(shape, focused, x, y, w, h, 1.0);
                assert!(
                    within(&rects, x, y, w, h),
                    "{shape:?} focused={focused}: {rects:?}"
                );
            }
        }
    }

    #[test]
    fn block_matches_the_cell_and_bar_and_underline_are_one_device_pixel() {
        let block = rectangles(Shape::Block, true, 0.0, 0.0, 8.0, 20.0, 1.0);
        assert_eq!(
            block,
            vec![Rect {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 20.0
            }]
        );
        let bar = rectangles(Shape::Bar, true, 0.0, 0.0, 8.0, 20.0, 1.0);
        assert!(near(bar[0].width, 1.0));
        assert!(near(bar[0].height, 20.0));
        let underline = rectangles(Shape::Underline, true, 0.0, 0.0, 8.0, 20.0, 1.0);
        assert!(near(underline[0].y, 19.0));
        assert!(near(underline[0].height, 1.0));
        assert!((stroke(2.0) - 1.0).abs() < f32::EPSILON);
        assert!((stroke(1.5) - 2.0 / 1.5).abs() < 1e-6);
        assert!((stroke(f32::NAN) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn unfocused_cursor_is_a_hollow_outline_of_the_same_cell() {
        let rects = rectangles(Shape::Block, false, 2.0, 4.0, 8.0, 20.0, 1.0);
        assert_eq!(rects.len(), 4);
        assert!(within(&rects, 2.0, 4.0, 8.0, 20.0));
        assert!(rects.iter().any(|r| near(r.y, 4.0) && near(r.height, 1.0)));
        assert!(rects.iter().any(|r| near(r.y, 23.0) && near(r.height, 1.0)));
    }
}
