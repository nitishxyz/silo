use crate::{
    CellMetrics,
    cursor_geometry::{Shape, rectangles},
    prepare::{CursorCell, CursorContent, PreparedText},
};
use gpui::{Bounds, Hsla, Path, Pixels, Point, Window, fill, point, px, rgb, size};
use silo_vt::{Cursor, CursorShape};

#[derive(Clone, Copy)]
pub struct CursorPresentation {
    pub focused: bool,
    pub phase_visible: bool,
}

fn shape(shape: CursorShape) -> Shape {
    match shape {
        CursorShape::Bar => Shape::Bar,
        CursorShape::Block => Shape::Block,
        CursorShape::Underline => Shape::Underline,
        CursorShape::Hollow => Shape::Hollow,
    }
}

/// Paint the cursor over the grid. A focused block is opaque in the cursor
/// color, with the cell under it (`cell`, when it is the cursor's) drawn
/// again on top in [`cursor_text`] of the cursor color and `background`.
pub(super) fn paint(
    cursor: Cursor,
    presentation: CursorPresentation,
    cell: Option<&CursorCell>,
    background: u32,
    bounds: Bounds<Pixels>,
    metrics: CellMetrics,
    window: &mut Window,
) {
    if !cursor.visible || (presentation.focused && !presentation.phase_visible) {
        return;
    }
    let shape = shape(cursor.shape);
    let solid = presentation.focused && shape == Shape::Block;
    let cell = cell.filter(|c| solid && c.row == cursor.row && (c.column..c.column + c.cells).contains(&cursor.column));
    let (column, cells) = cell.map_or((cursor.column, 1), |c| (c.column, c.cells));
    let x = f32::from(bounds.left()) + f32::from(column) * metrics.width;
    let y = f32::from(bounds.top()) + f32::from(cursor.row) * metrics.height;
    let width = f32::from(cells) * metrics.width;
    let color: Hsla = rgb(cursor.color).into();
    for rect in rectangles(shape, presentation.focused, x, y, width, metrics.height, metrics.scale) {
        window.paint_quad(fill(Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height))), color));
    }
    let Some(cell) = cell else { return };
    let ink: Hsla = rgb(cursor_text(cursor.color, background)).into();
    let origin = point(px(x), px(y));
    match &cell.content {
        CursorContent::Empty => {}
        CursorContent::Glyph(text) => {
            let text = PreparedText { colors: vec![(usize::MAX, ink)], origin: point(px(0.0), px(0.0)), ..text.clone() };
            crate::surface::paint_text(&text, origin, metrics, window);
        }
        CursorContent::Sprite(quads, polygons) => {
            for (quad, alpha) in quads {
                window.paint_quad(fill(Bounds::new(quad.origin + origin, quad.size), Hsla { a: ink.a * alpha, ..ink }));
            }
            for polygon in polygons {
                let mut vertices = polygon.iter().map(|v| *v + origin);
                let Some(first) = vertices.next() else { continue };
                let mut path: Path<Pixels> = Path::new(first);
                vertices.for_each(|v: Point<Pixels>| path.line_to(v));
                window.paint_path(path, ink);
            }
        }
    }
}

/// Near-black and near-white inks, as the app's chrome uses.
const DARK_INK: u32 = 0x001d_1d1f;
const LIGHT_INK: u32 = 0x00f5_f5f7;
/// WCAG minimum contrast for text.
const TEXT_CONTRAST: f32 = 4.5;

/// The color of the glyph under a solid block cursor of `cursor` color:
/// the terminal background, as Ghostty does, when it reads on the cursor;
/// otherwise whichever of near-black and near-white reads better.
#[must_use]
pub(crate) fn cursor_text(cursor: u32, background: u32) -> u32 {
    if contrast(background, cursor) >= TEXT_CONTRAST {
        return background;
    }
    let ink = if contrast(DARK_INK, cursor) >= contrast(LIGHT_INK, cursor) { DARK_INK } else { LIGHT_INK };
    if contrast(ink, cursor) > contrast(background, cursor) { ink } else { background }
}

/// WCAG relative luminance (linearized sRGB), 0-1.
fn relative_luminance(c: u32) -> f32 {
    let linear = |v: u32| {
        let v = (v & 0xff) as f32 / 255.0;
        if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(c >> 16) + 0.7152 * linear(c >> 8) + 0.0722 * linear(c)
}

/// WCAG contrast ratio, 1-21.
fn contrast(a: u32, b: u32) -> f32 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::{DARK_INK, LIGHT_INK, contrast, cursor_text};

    #[test]
    fn cursor_text_is_the_background_where_it_reads_else_an_ink() {
        // Dark theme, light cursor; light theme, dark cursor: the background.
        assert_eq!(cursor_text(0x00ab_b2bf, 0x0028_2c34), 0x0028_2c34);
        assert_eq!(cursor_text(0x0038_3a42, 0x00fa_fafa), 0x00fa_fafa);
        // A cursor about the background's color: an ink that reads on it.
        assert_eq!(cursor_text(0x0028_2c34, 0x0028_2c34), LIGHT_INK);
        assert_eq!(cursor_text(0x00f0_f0f0, 0x00ff_ffff), DARK_INK);
        // A mid-tone accent on a dark background: the darker ink, not the background.
        let ink = cursor_text(0x0052_8bff, 0x0028_2c34);
        assert_eq!(ink, DARK_INK);
        assert!(contrast(ink, 0x0052_8bff) > contrast(0x0028_2c34, 0x0052_8bff));
        // Whatever is chosen reads at least as well as the background.
        for (cursor, background) in [(0x00ff_0000, 0x0000_0000), (0x0080_8080, 0x0080_8080), (0x00ff_cc00, 0x00ff_ffff)] {
            let ink = cursor_text(cursor, background);
            assert!(contrast(ink, cursor) >= contrast(background, cursor), "{cursor:06x} on {background:06x}: {ink:06x}");
        }
    }
}
