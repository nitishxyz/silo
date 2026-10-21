use std::sync::Arc;

use gpui::{App, Bounds, Hsla, IntoElement, Path, Pixels, Point, Rgba, TextAlign, TextRun, Window, canvas, fill, point, prelude::*, px, rgb, size};
use silo_vt::Span;

use crate::geometry::CellMetrics;
use crate::prepare::{PreparedFrame, PreparedText, grid_offset};
use crate::profile;

/// Platform-owned highlights painted over the grid: search matches as a
/// translucent tint (the selected match is already Ghostty's selection) and
/// one hovered link underlined in its own cell colors.
///
/// Overlays are at most a few dozen spans and change with the pointer, not
/// with output, so they stay on the UI thread rather than forcing a frame
/// to be prepared again.
#[derive(Clone, Default)]
pub struct Overlay {
    pub matches: Vec<Span>,
    pub match_color: Rgba,
    pub link: Option<Span>,
    /// Predicted, unconfirmed cells (docs/PHASE5.md §5).
    pub predicted: Vec<Predicted>,
    /// Painted instead of the frame's cursor position while set.
    pub predicted_cursor: Option<(u16, u16)>,
    /// The terminal's default foreground, for predicted glyphs.
    pub predicted_foreground: Rgba,
    /// The 1 px underline under predicted cells (`theme::muted()`).
    pub predicted_underline: Rgba,
    /// Viewport rows of failed commands' D marks: a tick in the gutter
    /// left of the grid (`docs/PHASE7.md` §7.4).
    pub failed: Vec<u16>,
    pub failed_color: Rgba,
}

/// One predicted cell in viewport coordinates; empty `text` is a blank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Predicted {
    pub row: u16,
    pub column: u16,
    pub text: String,
}

impl Overlay {
    fn span_bounds(span: Span, bounds: Bounds<Pixels>, columns: u16, rows: u16, metrics: CellMetrics) -> Option<Bounds<Pixels>> {
        let (x, y, width, height) =
            crate::span_geometry::span_rectangle(span.row, span.start, span.end, columns, rows, metrics.width, metrics.height)?;
        Some(Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(width), px(height))))
    }

    /// Cell and underline bounds of every predicted cell inside the grid.
    fn predicted_quads(
        &self,
        bounds: Bounds<Pixels>,
        columns: u16,
        rows: u16,
        metrics: CellMetrics,
    ) -> Vec<(&Predicted, Bounds<Pixels>, Bounds<Pixels>)> {
        let place = |(x, y, w, h): crate::predict_geometry::Rect| Bounds::new(point(bounds.left() + px(x), bounds.top() + px(y)), size(px(w), px(h)));
        self.predicted
            .iter()
            .filter_map(|p| {
                let (cell, line) =
                    crate::predict_geometry::predicted_rects(p.row, p.column, columns, rows, metrics.width, metrics.height, metrics.scale)?;
                Some((p, place(cell), place(line)))
            })
            .collect()
    }

    /// The gutter ticks: 2 px wide, [`TICK_GAP`] left of the grid, one row
    /// tall; rows outside the grid are skipped.
    fn tick_quads(&self, bounds: Bounds<Pixels>, rows: u16, metrics: CellMetrics) -> Vec<Bounds<Pixels>> {
        self.failed
            .iter()
            .filter(|&&row| row < rows)
            .map(|&row| {
                let top = bounds.top() + px(f32::from(row) * metrics.height);
                Bounds::new(point(bounds.left() - px(TICK_GAP), top), size(px(2.0), px(metrics.height)))
            })
            .collect()
    }
}

/// How far left of the grid a failed command's tick is painted, in the
/// pane's padding.
pub const TICK_GAP: f32 = 4.0;

/// Prepared geometry is pane-relative; paint places it at the pane's origin.
fn shift(bounds: Bounds<Pixels>, origin: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::new(bounds.origin + origin, bounds.size)
}

fn polygon_path(points: &[Point<Pixels>]) -> Option<Path<Pixels>> {
    let mut vertices = points.iter().copied();
    let mut path = Path::new(vertices.next()?);
    for vertex in vertices {
        path.line_to(vertex);
    }
    Some(path)
}

/// Mirrors GPUI's own line painting (baseline from the line height and the
/// layout's ascent/descent), but positions each cluster at its cell.
pub(crate) fn paint_text(text: &PreparedText, origin: Point<Pixels>, metrics: CellMetrics, window: &mut Window) {
    let layout = &text.layout;
    let padding_top = (px(metrics.height) - layout.ascent - layout.descent) / 2.0;
    let baseline = origin.y + text.origin.y + padding_top + layout.ascent;
    let mut cell = 0;
    let mut cluster_x = px(0.0);
    let mut placed = false;
    for run in &layout.runs {
        for glyph in &run.glyphs {
            // Glyph order may differ from byte order (bidi), so look the
            // color up by byte rather than walking segments forward.
            let segment = text.colors.partition_point(|(end, _)| *end <= glyph.index).min(text.colors.len().saturating_sub(1));
            let color = text.colors.get(segment).map_or_else(Hsla::default, |c| c.1);
            if !text.starts.is_empty() {
                let owner = crate::runs::cell_of(&text.starts, glyph.index);
                if !placed || owner != cell {
                    cell = owner;
                    cluster_x = glyph.position.x;
                    placed = true;
                }
            }
            let x = origin.x + text.origin.x + text.inset + px(grid_offset(cell) * metrics.width) + (glyph.position.x - cluster_x);
            let glyph_origin = point(x, baseline + glyph.position.y);
            let _ = if glyph.is_emoji {
                window.paint_emoji(glyph_origin, run.font_id, glyph.id, layout.font_size)
            } else {
                window.paint_glyph(glyph_origin, run.font_id, glyph.id, layout.font_size, color)
            };
        }
    }
}

fn paint(prepared: &PreparedFrame, overlay: &Overlay, bounds: Bounds<Pixels>, metrics: CellMetrics, selection: Rgba, window: &mut Window) {
    window.with_content_mask(Some(gpui::ContentMask::new(bounds)), |window| {
        // One paint layer for the whole grid. GPUI otherwise assigns every
        // primitive its own z-order by inserting its bounds into the scene's
        // R-tree, which is the dominant UI-thread cost of a Braille-heavy
        // frame (see docs/terminal-gpu-rendering.md): the tree is touched
        // once per layer instead of ~9500 times per frame. Inside the layer
        // GPUI draws by primitive kind — quads, then paths, then glyph
        // sprites — and preserves insertion order within each kind, which is
        // the order this function already paints in. Underlines and
        // strikethroughs therefore layer beneath glyphs rather than over
        // them, which is what Ghostty does deliberately for readability.
        window.paint_layer(bounds, |window| {
            paint_grid(prepared, overlay, bounds, metrics, selection, window);
        });
    });
}

fn paint_grid(prepared: &PreparedFrame, overlay: &Overlay, bounds: Bounds<Pixels>, metrics: CellMetrics, selection: Rgba, window: &mut Window) {
    let origin = bounds.origin;
    let cell = prepared.metrics;
    let frame = &prepared.frame;
    let span_bounds = |span: Span| Overlay::span_bounds(span, bounds, frame.size.columns, frame.size.rows, cell);
    for (quad, color) in &prepared.backgrounds {
        window.paint_quad(fill(shift(*quad, origin), *color));
    }
    for quad in overlay.matches.iter().copied().filter_map(span_bounds) {
        window.paint_quad(fill(quad, overlay.match_color));
    }
    for quad in &prepared.selection {
        window.paint_quad(fill(shift(*quad, origin), selection));
    }
    for (quad, color) in &prepared.sprite_quads {
        window.paint_quad(fill(shift(*quad, origin), *color));
    }
    for (polygon, color) in &prepared.sprite_polygons {
        let translated: Vec<Point<Pixels>> = polygon.iter().map(|vertex| *vertex + origin).collect();
        if let Some(path) = polygon_path(&translated) {
            window.paint_path(path, *color);
        }
    }
    for text in &prepared.text {
        paint_text(text, origin, cell, window);
    }
    for (line, color) in &prepared.lines {
        window.paint_quad(fill(shift(*line, origin), *color));
    }
    if let Some(span) = overlay.link
        && let Some(link) = span_bounds(span)
    {
        let columns = usize::from(frame.size.columns).max(1);
        let color = frame.cells.get(usize::from(span.row) * columns + usize::from(span.start)).map_or(0xffff_ffff, |cell| cell.foreground);
        window.paint_quad(fill(Bounds::new(point(link.left(), link.bottom() - px(2.0)), size(link.size.width, px(1.0 / metrics.scale))), rgb(color)));
    }
}

/// Predicted cells in their own layer above the grid, so the background
/// covers the mirror's glyph: default background (none over a frosted
/// card, whose veil is the default background), default foreground glyph,
/// and the muted underline as the only cue.
fn paint_predicted(prepared: &PreparedFrame, overlay: &Overlay, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let frame = &prepared.frame;
    let quads = overlay.predicted_quads(bounds, frame.size.columns, frame.size.rows, prepared.metrics);
    if quads.is_empty() {
        return;
    }
    let mask = Some(gpui::ContentMask::new(bounds));
    window.with_content_mask(mask, |window| {
        window.paint_layer(bounds, |window| {
            for (cell, quad, line) in quads {
                if crate::cell_alpha(frame.background, frame.background, prepared.translucency).is_some() {
                    window.paint_quad(fill(quad, rgb(frame.background)));
                }
                window.paint_quad(fill(line, overlay.predicted_underline));
                if cell.text.is_empty() {
                    continue;
                }
                let run = TextRun {
                    len: cell.text.len(),
                    font: crate::terminal_font(),
                    color: overlay.predicted_foreground.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(cell.text.clone().into(), px(prepared.metrics.font_size), &[run], None);
                let _ = shaped.paint(quad.origin, quad.size.height, TextAlign::Left, None, window, cx);
            }
        })
    });
}

/// The cell under a predicted cursor, laid out on the UI thread.
fn predicted_cursor_cell(prepared: &PreparedFrame, position: (u16, u16), window: &mut Window) -> Option<crate::prepare::CursorCell> {
    let metrics = prepared.metrics;
    let text = window.text_system().clone();
    crate::prepare::cursor_cell_at(
        &prepared.frame,
        position,
        metrics,
        &mut |cell_text, bold, italic| {
            let run = TextRun {
                len: cell_text.len(),
                font: crate::prepare::face(bold, italic),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            text.layout_line(cell_text, px(metrics.font_size), &[run], None)
        },
        &mut |code, width| crate::sprite::sprite(code, 0.0, 0.0, width, metrics.height, metrics.scale).map(|s| crate::prepare::sprite_content(&s)),
    )
}

/// One canvas for the complete grid; no GPUI element or event handler per
/// cell, and no layout work in the prepaint phase: `prepared` already holds
/// every span, sprite and shaped run, built off the UI thread.
///
/// The prepared frame also supplies the cursor and the hovered link's
/// color; `selection` is the theme's translucent selection tint; `overlay`
/// carries search-match tints and the hovered link underline.
#[must_use]
pub fn terminal_canvas(
    prepared: Arc<PreparedFrame>,
    metrics: CellMetrics,
    cursor: crate::CursorPresentation,
    selection: Rgba,
    overlay: Overlay,
) -> impl IntoElement {
    canvas(
        move |_, _, _| (prepared, overlay),
        move |bounds, (prepared, overlay), window, cx| {
            let started = profile::start();
            paint(&prepared, &overlay, bounds, metrics, selection, window);
            paint_predicted(&prepared, &overlay, bounds, window, cx);
            for tick in overlay.tick_quads(bounds, prepared.frame.size.rows, prepared.metrics) {
                window.paint_quad(fill(tick, overlay.failed_color));
            }
            let (quads, glyphs) = if profile::enabled() { prepared.primitives() } else { (0, 0) };
            profile::record_painted(started, quads, glyphs);
            let mut caret = prepared.frame.cursor;
            if let Some((row, column)) = overlay.predicted_cursor {
                (caret.row, caret.column) = (row, column);
            }
            window.with_content_mask(Some(gpui::ContentMask::new(bounds)), |window| {
                // A predicted cursor covers another cell than the prepared one:
                // look that one up (only while predicting, one cell).
                let predicted = overlay
                    .predicted_cursor
                    .filter(|_| cursor.focused && caret.visible && caret.shape == silo_vt::CursorShape::Block)
                    .and_then(|position| predicted_cursor_cell(&prepared, position, window));
                let cell = if overlay.predicted_cursor.is_some() { predicted.as_ref() } else { prepared.cursor_cell.as_ref() };
                crate::cursor::paint(caret, cursor, cell, prepared.frame.background, bounds, prepared.metrics, window);
            });
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_sit_in_the_gutter_on_their_rows() {
        let metrics = CellMetrics { width: 8.0, height: 20.0, font_size: 14.0, scale: 2.0, font: 0 };
        let bounds = Bounds::new(point(px(100.0), px(50.0)), size(px(800.0), px(480.0)));
        let overlay = Overlay { failed: vec![0, 3, 24], ..Overlay::default() };
        let ticks = overlay.tick_quads(bounds, 24, metrics);
        assert_eq!(
            ticks,
            vec![Bounds::new(point(px(96.0), px(50.0)), size(px(2.0), px(20.0))), Bounds::new(point(px(96.0), px(110.0)), size(px(2.0), px(20.0))),]
        );
    }

    #[test]
    fn underline_only_under_predicted_cells() {
        let metrics = CellMetrics { width: 8.0, height: 20.0, font_size: 14.0, scale: 2.0, font: 0 };
        let bounds = Bounds::new(point(px(100.0), px(50.0)), size(px(800.0), px(480.0)));
        assert!(Overlay::default().predicted_quads(bounds, 80, 24, metrics).is_empty());
        let cell = |column, text: &str| Predicted { row: 1, column, text: text.into() };
        let overlay = Overlay { predicted: vec![cell(4, "a"), cell(5, ""), cell(80, "x")], ..Overlay::default() };
        let quads = overlay.predicted_quads(bounds, 80, 24, metrics);
        assert_eq!(quads.len(), 2, "off-grid cell skipped");
        for ((p, quad, line), x) in quads.into_iter().zip([132.0, 140.0]) {
            assert_eq!(f32::from(p.column), (x - 100.0) / 8.0);
            assert_eq!(quad, Bounds::new(point(px(x), px(70.0)), size(px(8.0), px(20.0))));
            assert_eq!(line, Bounds::new(point(px(x), px(88.0)), size(px(8.0), px(0.5))));
        }
    }
}
