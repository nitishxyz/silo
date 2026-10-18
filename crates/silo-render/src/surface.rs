use std::sync::Arc;

use gpui::{
    Bounds, Hsla, IntoElement, Path, Pixels, Point, Rgba, Window, canvas, fill, point, prelude::*,
    px, rgb, size,
};
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
}

impl Overlay {
    fn span_bounds(
        span: Span,
        bounds: Bounds<Pixels>,
        columns: u16,
        rows: u16,
        metrics: CellMetrics,
    ) -> Option<Bounds<Pixels>> {
        let (x, y, width, height) = crate::span_geometry::span_rectangle(
            span.row,
            span.start,
            span.end,
            columns,
            rows,
            metrics.width,
            metrics.height,
        )?;
        Some(Bounds::new(
            point(bounds.left() + px(x), bounds.top() + px(y)),
            size(px(width), px(height)),
        ))
    }
}

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
fn paint_text(
    text: &PreparedText,
    origin: Point<Pixels>,
    metrics: CellMetrics,
    window: &mut Window,
) {
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
            let segment = text
                .colors
                .partition_point(|(end, _)| *end <= glyph.index)
                .min(text.colors.len().saturating_sub(1));
            let color = text.colors.get(segment).map_or_else(Hsla::default, |c| c.1);
            if !text.starts.is_empty() {
                let owner = crate::runs::cell_of(&text.starts, glyph.index);
                if !placed || owner != cell {
                    cell = owner;
                    cluster_x = glyph.position.x;
                    placed = true;
                }
            }
            let x = origin.x
                + text.origin.x
                + text.inset
                + px(grid_offset(cell) * metrics.width)
                + (glyph.position.x - cluster_x);
            let glyph_origin = point(x, baseline + glyph.position.y);
            let _ = if glyph.is_emoji {
                window.paint_emoji(glyph_origin, run.font_id, glyph.id, layout.font_size)
            } else {
                window.paint_glyph(glyph_origin, run.font_id, glyph.id, layout.font_size, color)
            };
        }
    }
}

fn paint(
    prepared: &PreparedFrame,
    overlay: &Overlay,
    bounds: Bounds<Pixels>,
    metrics: CellMetrics,
    selection: Rgba,
    window: &mut Window,
) {
    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
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

fn paint_grid(
    prepared: &PreparedFrame,
    overlay: &Overlay,
    bounds: Bounds<Pixels>,
    metrics: CellMetrics,
    selection: Rgba,
    window: &mut Window,
) {
    let origin = bounds.origin;
    let cell = prepared.metrics;
    let frame = &prepared.frame;
    let span_bounds =
        |span: Span| Overlay::span_bounds(span, bounds, frame.size.columns, frame.size.rows, cell);
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
        let translated: Vec<Point<Pixels>> =
            polygon.iter().map(|vertex| *vertex + origin).collect();
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
        let color = frame
            .cells
            .get(usize::from(span.row) * columns + usize::from(span.start))
            .map_or(0xffff_ffff, |cell| cell.foreground);
        window.paint_quad(fill(
            Bounds::new(
                point(link.left(), link.bottom() - px(2.0)),
                size(link.size.width, px(1.0 / metrics.scale)),
            ),
            rgb(color),
        ));
    }
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
        move |bounds, (prepared, overlay), window, _| {
            let started = profile::start();
            paint(&prepared, &overlay, bounds, metrics, selection, window);
            let (quads, glyphs) = if profile::enabled() {
                prepared.primitives()
            } else {
                (0, 0)
            };
            profile::record_painted(started, quads, glyphs);
            window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                crate::cursor::paint(prepared.frame.cursor, cursor, bounds, metrics, window);
            });
        },
    )
    .size_full()
}
