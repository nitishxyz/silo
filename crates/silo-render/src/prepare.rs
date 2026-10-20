//! Render-ready frame preparation, away from the UI thread.
//!
//! Everything a frame needs in order to be painted — background and
//! selection spans, underline/strikethrough hairlines, procedural sprite
//! geometry and shaped text runs — is built here from an owned [`Frame`],
//! in pane-relative coordinates and without a [`gpui::Window`]. The UI
//! thread then only translates the result by the pane origin and issues the
//! GPUI paint calls, so a busy TUI cannot make keystrokes wait behind
//! layout work.
//!
//! Text shaping uses a private [`WindowTextSystem`] over the application's
//! shared [`TextSystem`]: font ids and glyph ids stay the ones the window's
//! atlas rasterizes, while the layout cache is owned by the preparing
//! thread instead of the window's per-frame one.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use gpui::{Bounds, Font, FontStyle, FontWeight, Hsla, LineLayout, Pixels, Point, TextRun, TextSystem, WindowTextSystem, point, px, rgb, size};
use silo_vt::{Cell, Frame};

use crate::{
    geometry::CellMetrics,
    profile, quads,
    runs::{CellView, Run, Single, plan_row},
    sprite,
};

/// One shaped line, either a run of narrow cells or a single wide cell.
///
/// The layout is shaped as ordinary text and painted glyph by glyph: each
/// cluster is placed at its own cell origin, exactly as per-cell shaping
/// placed it, so batching changes the amount of shaping work, never glyph
/// positions.
#[derive(Clone)]
pub(crate) struct PreparedText {
    pub layout: Arc<LineLayout>,
    /// Top-left of the first cell, relative to the pane's content origin.
    pub origin: Point<Pixels>,
    /// Extra x inside the first cell (centring a wide glyph); zero for runs.
    pub inset: Pixels,
    /// Byte offset where each cell's grapheme starts; empty for a single cell.
    pub starts: Vec<usize>,
    /// `(end byte, color)` per color segment in text order.
    pub colors: Vec<(usize, Hsla)>,
}

/// A frame's complete paint geometry, in coordinates relative to the pane's
/// content origin. Paint order: backgrounds, selection, sprite quads, sprite
/// polygons, text, then hairlines.
pub struct PreparedFrame {
    /// The frame this geometry was built from. Paint reads the cursor and
    /// hovered-link colors from it, so what is painted is always one
    /// consistent frame even when a newer one has already arrived.
    pub(crate) frame: Arc<Frame>,
    pub(crate) metrics: CellMetrics,
    pub(crate) translucency: Option<Translucency>,
    pub(crate) backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    pub(crate) selection: Vec<Bounds<Pixels>>,
    pub(crate) sprite_quads: Vec<(Bounds<Pixels>, Hsla)>,
    pub(crate) sprite_polygons: Vec<(Vec<Point<Pixels>>, Hsla)>,
    pub(crate) lines: Vec<(Bounds<Pixels>, Hsla)>,
    pub(crate) text: Vec<PreparedText>,
    /// The content of the cell under a block cursor, repainted in the
    /// cursor-text color over the opaque cursor.
    pub(crate) cursor_cell: Option<CursorCell>,
}

/// The cell a block cursor sits on: where it is, how many columns it covers
/// (2 for a wide character) and its glyph or sprite geometry, colorless so
/// paint can choose the cursor-text color.
pub(crate) struct CursorCell {
    pub row: u16,
    /// First column of the cell (the wide character, not its spacer).
    pub column: u16,
    pub cells: u16,
    pub content: CursorContent,
}

pub(crate) enum CursorContent {
    Empty,
    Glyph(PreparedText),
    /// Sprite quads and polygons with their coverage, relative to the cell.
    Sprite(Vec<(Bounds<Pixels>, f32)>, Vec<Vec<Point<Pixels>>>),
}

impl PreparedFrame {
    /// The terminal frame this geometry was built from (what is painted).
    #[must_use]
    pub fn frame(&self) -> &Arc<Frame> {
        &self.frame
    }

    /// Quads and glyphs the UI thread will issue for this frame.
    pub(crate) fn primitives(&self) -> (u64, u64) {
        let quads = self.backgrounds.len() + self.selection.len() + self.sprite_quads.len();
        let glyphs: usize = self.text.iter().map(|text| text.layout.runs.iter().map(|run| run.glyphs.len()).sum::<usize>()).sum();
        (u64::try_from(quads + self.lines.len()).unwrap_or(u64::MAX), u64::try_from(glyphs).unwrap_or(u64::MAX))
    }
}

/// Per-frame choices that come from the user's configuration rather than
/// from the terminal itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameOptions {
    /// `selection-foreground`: the text color of selected cells. `None`
    /// leaves selected text in its own colors under the selection tint.
    pub selection_foreground: Option<u32>,
    /// Over a blurred window: how translucent cell backgrounds are.
    pub translucency: Option<Translucency>,
}

/// Cell background opacity over a frosted card. The terminal's default
/// background, the card's colour and `veiled` (an app's own backdrop) are
/// left to the card's veil; every other background is painted at
/// `opaque_alpha`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translucency {
    pub card: u32,
    pub veiled: Option<u32>,
    pub opaque_alpha: u8,
}

/// The alpha (0–255) to paint an explicit cell background of `color` at,
/// or `None` to leave it to the card. Without translucency, opaque.
#[must_use]
pub fn cell_alpha(color: u32, default_background: u32, translucency: Option<Translucency>) -> Option<u8> {
    let Some(t) = translucency else { return Some(u8::MAX) };
    (color != default_background && color != t.card && Some(color) != t.veiled).then_some(t.opaque_alpha)
}

/// Builds [`PreparedFrame`]s. One per pane, owned by whatever thread
/// prepares its frames; never touched by the UI thread.
pub struct FramePreparer {
    layouts: LayoutCache,
    sprites: sprite::SpriteCache,
    /// The font generation the cached layouts were shaped with.
    font: u32,
}

impl FramePreparer {
    /// `text` is the application-wide text system (`cx.text_system()`), so
    /// font ids resolve to the same faces the window paints with.
    #[must_use]
    pub fn new(text: Arc<TextSystem>) -> Self {
        Self { layouts: LayoutCache::new(text), sprites: sprite::SpriteCache::default(), font: crate::font_generation() }
    }

    #[must_use]
    pub fn prepare(&mut self, frame: &Arc<Frame>, metrics: CellMetrics, options: FrameOptions) -> PreparedFrame {
        let started = profile::start();
        let mut cost = profile::Prepared::default();
        // A configured font family changes the faces without changing the
        // text, so cached layouts have to go with it.
        let font = crate::font_generation();
        if font != self.font {
            self.font = font;
            self.layouts.forget();
        }
        let selection = options.selection_foreground;
        let columns = usize::from(frame.size.columns).max(1);
        let mut out = PreparedFrame {
            frame: frame.clone(),
            metrics,
            translucency: options.translucency,
            backgrounds: Vec::new(),
            selection: Vec::new(),
            sprite_quads: Vec::new(),
            sprite_polygons: Vec::new(),
            lines: Vec::new(),
            text: Vec::new(),
            cursor_cell: None,
        };
        let mut views: Vec<CellView<'_>> = Vec::with_capacity(columns);
        for (row, cells) in frame.cells.chunks(columns).enumerate() {
            let y = px(grid_offset(row) * metrics.height);
            push_backgrounds(&mut out, cells, y, metrics, frame.background, options.translucency);
            push_selection(&mut out, cells, y, metrics);
            push_decorations(&mut out, cells, y, metrics, selection);
            let planning = profile::start();
            views.clear();
            views.extend(cells.iter().map(|cell| CellView {
                text: &cell.text,
                width: cell.width,
                sprite: sprite::code_point(&cell.text).is_some(),
                bold: cell.bold,
                italic: cell.italic,
                faint: cell.faint,
                foreground: cell_foreground(cell, selection),
            }));
            let plan = plan_row(&views);
            profile::add(&mut cost.plan, planning);
            for single in plan.singles {
                let column = match single {
                    Single::Sprite(column) | Single::Glyph(column) => column,
                };
                let clip = cell_bounds(y, column, usize::from(cells[column].width), metrics);
                match single {
                    Single::Sprite(_) => {
                        let sprites = profile::start();
                        push_sprite_cell(&mut out, &mut self.sprites, &cells[column], clip, selection);
                        profile::add(&mut cost.sprites, sprites);
                    }
                    Single::Glyph(_) => {
                        let shaping = profile::start();
                        let text = shape_single(&mut self.layouts, &cells[column], clip, metrics, selection, &mut cost);
                        profile::add(&mut cost.shape, shaping);
                        out.text.push(text);
                    }
                }
            }
            for run in plan.runs {
                let origin = point(px(grid_offset(run.column) * metrics.width), y);
                let shaping = profile::start();
                let text = shape_run(&mut self.layouts, run, origin, metrics, &mut cost);
                profile::add(&mut cost.shape, shaping);
                out.text.push(text);
            }
        }
        out.cursor_cell = cursor_cell(frame, metrics, &mut self.layouts, &mut self.sprites, &mut cost);
        self.layouts.end_frame();
        profile::add(&mut cost.total, started);
        profile::record_prepared(&cost);
        out
    }
}

/// Shaped lines reused across prepared frames.
///
/// GPUI's own layout cache belongs to the window and is evicted per painted
/// frame from the UI thread, so preparation keeps its own two-generation
/// cache: a row that survives one frame is a hash lookup instead of a
/// reshape. The private `WindowTextSystem` behind it also caches internally
/// and is only evicted by the window loop, so it is recycled after enough
/// misses; the layouts this cache still holds survive that untouched.
struct LayoutCache {
    system: Arc<TextSystem>,
    text: Arc<WindowTextSystem>,
    misses: usize,
    font_size: f32,
    current: HashMap<u64, Entry>,
    previous: HashMap<u64, Entry>,
}

struct Entry {
    text: String,
    bold: bool,
    italic: bool,
    layout: Arc<LineLayout>,
}

impl Entry {
    fn matches(&self, text: &str, bold: bool, italic: bool) -> bool {
        self.bold == bold && self.italic == italic && self.text == text
    }
}

/// Misses after which the private text system (and its unbounded internal
/// layout cache) is replaced.
const RECYCLE_AFTER: usize = 1024;

impl LayoutCache {
    fn new(system: Arc<TextSystem>) -> Self {
        Self {
            text: Arc::new(WindowTextSystem::new(system.clone())),
            system,
            misses: 0,
            font_size: 0.0,
            current: HashMap::new(),
            previous: HashMap::new(),
        }
    }

    fn layout(&mut self, text: &str, bold: bool, italic: bool, font_size: f32, cost: &mut profile::Prepared) -> Arc<LineLayout> {
        if (self.font_size - font_size).abs() > f32::EPSILON {
            self.font_size = font_size;
            self.current.clear();
            self.previous.clear();
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        (bold, italic).hash(&mut hasher);
        let key = hasher.finish();
        if let Some(entry) = self.current.get(&key)
            && entry.matches(text, bold, italic)
        {
            cost.reused += 1;
            return entry.layout.clone();
        }
        if let Some(entry) = self.previous.remove(&key)
            && entry.matches(text, bold, italic)
        {
            cost.reused += 1;
            let layout = entry.layout.clone();
            self.current.insert(key, entry);
            return layout;
        }
        cost.shaped += 1;
        self.misses += 1;
        if self.misses >= RECYCLE_AFTER {
            self.misses = 0;
            self.text = Arc::new(WindowTextSystem::new(self.system.clone()));
        }
        let run = TextRun {
            len: text.len(),
            font: face(bold, italic),
            // Color is applied per glyph at paint time, so the layout is
            // keyed by the text and the face alone.
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = self.text.layout_line(text, px(font_size), &[run], None);
        self.current.insert(key, Entry { text: text.to_owned(), bold, italic, layout: layout.clone() });
        layout
    }

    /// Drop every cached layout, e.g. when the configured font changed.
    fn forget(&mut self) {
        self.current.clear();
        self.previous.clear();
        self.text = Arc::new(WindowTextSystem::new(self.system.clone()));
        self.misses = 0;
    }

    /// Two-generation retention, as GPUI's own line layout cache does.
    fn end_frame(&mut self) {
        self.previous.clear();
        std::mem::swap(&mut self.previous, &mut self.current);
    }
}

pub(crate) fn grid_offset(index: usize) -> f32 {
    f32::from(u16::try_from(index).unwrap_or(u16::MAX))
}

pub(crate) fn face(bold: bool, italic: bool) -> Font {
    let mut font = crate::terminal_font();
    if bold {
        font.weight = FontWeight::BOLD;
    }
    if italic {
        font.style = FontStyle::Italic;
    }
    font
}

pub(crate) fn text_color(foreground: u32, faint: bool) -> Hsla {
    let mut color: Hsla = rgb(foreground).into();
    if faint {
        color.a = 0.6;
    }
    color
}

/// Push a quad, joining it onto the previous one when the two tile exactly
/// and share a color; that keeps whole rows of box drawing, blocks and
/// underlines down to one paint call each.
fn push_joined(out: &mut Vec<(Bounds<Pixels>, Hsla)>, bounds: Bounds<Pixels>, color: Hsla) {
    if let Some((last, last_color)) = out.last_mut()
        && *last_color == color
        && quads::joins(rect_of(*last), rect_of(bounds))
    {
        *last = bounds_of(quads::join(rect_of(*last), rect_of(bounds)));
        return;
    }
    out.push((bounds, color));
}

fn rect_of(bounds: Bounds<Pixels>) -> quads::Rect {
    quads::Rect {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

fn bounds_of(rect: quads::Rect) -> Bounds<Pixels> {
    Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height)))
}

fn cell_bounds(y: Pixels, column: usize, cells: usize, metrics: CellMetrics) -> Bounds<Pixels> {
    Bounds::new(point(px(grid_offset(column) * metrics.width), y), size(px(grid_offset(cells) * metrics.width), px(metrics.height)))
}

fn push_backgrounds(out: &mut PreparedFrame, cells: &[Cell], y: Pixels, metrics: CellMetrics, default: u32, translucency: Option<Translucency>) {
    let mut start = 0;
    while start < cells.len() {
        if !cells[start].background_explicit {
            start += 1;
            continue;
        }
        let color = cells[start].background;
        let mut end = start + 1;
        while end < cells.len() && cells[end].background_explicit && cells[end].background == color {
            end += 1;
        }
        if let Some(alpha) = cell_alpha(color, default, translucency) {
            out.backgrounds.push((
                Bounds::new(point(px(grid_offset(start) * metrics.width), y), size(px(grid_offset(end - start) * metrics.width), px(metrics.height))),
                rgb(color).opacity(f32::from(alpha) / 255.0).into(),
            ));
        }
        start = end;
    }
}

/// Selection is a translucent theme tint painted over the cell's own
/// background and under its glyph, so it reads on either material and
/// never recolors text a program chose.
fn push_selection(out: &mut PreparedFrame, cells: &[Cell], y: Pixels, metrics: CellMetrics) {
    let mut start = 0;
    while start < cells.len() {
        if !cells[start].selected {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < cells.len() && cells[end].selected {
            end += 1;
        }
        out.selection.push(Bounds::new(
            point(px(grid_offset(start) * metrics.width), y),
            size(px(grid_offset(end - start) * metrics.width), px(metrics.height)),
        ));
        start = end;
    }
}

/// Underline and strikethrough are hairline quads per cell span, painted
/// after the text so they are independent of how cells were batched.
fn push_decorations(out: &mut PreparedFrame, cells: &[Cell], y: Pixels, metrics: CellMetrics, selection: Option<u32>) {
    for (column, cell) in cells.iter().enumerate() {
        if cell.width == 0 || !(cell.underline || cell.strikethrough) {
            continue;
        }
        let clip = cell_bounds(y, column, usize::from(cell.width), metrics);
        for line_y in [cell.underline.then_some(clip.bottom() - px(2.0)), cell.strikethrough.then_some(clip.top() + px(metrics.height * 0.55))]
            .into_iter()
            .flatten()
        {
            push_joined(
                &mut out.lines,
                Bounds::new(point(clip.left(), line_y), size(clip.size.width, px(1.0 / metrics.scale))),
                rgb(cell_foreground(cell, selection)).into(),
            );
        }
    }
}

/// Text color of one cell before the faint adjustment: a configured
/// `selection-foreground` replaces the cell's own color while it is selected.
fn cell_foreground(cell: &Cell, selection: Option<u32>) -> u32 {
    match selection {
        Some(color) if cell.selected => color,
        _ => cell.foreground,
    }
}

/// Block, box-drawing, Braille and Powerline cells are exact cell geometry,
/// never font glyphs, so rows and columns tile without seams. The shape is
/// cached per (code point, cell size) and translated into the cell here.
fn push_sprite_cell(out: &mut PreparedFrame, cache: &mut sprite::SpriteCache, cell: &Cell, clip: Bounds<Pixels>, selection: Option<u32>) {
    let color = text_color(cell_foreground(cell, selection), cell.faint);
    let metrics = out.metrics;
    let Some(code) = sprite::code_point(&cell.text) else {
        return;
    };
    let Some(shape) = cache.geometry(code, f32::from(clip.size.width), metrics.height, metrics.scale) else {
        return;
    };
    for quad in &shape.quads {
        let mut color = color;
        color.a *= quad.alpha;
        // Cells are prepared left to right, so a rule, border or block run
        // joins onto the rectangle its neighbour just pushed.
        push_joined(
            &mut out.sprite_quads,
            Bounds::new(point(clip.left() + px(quad.x), clip.top() + px(quad.y)), size(px(quad.width), px(quad.height))),
            color,
        );
    }
    for polygon in &shape.polygons {
        out.sprite_polygons.push((polygon.0.iter().map(|(x, y)| point(clip.left() + px(*x), clip.top() + px(*y))).collect(), color));
    }
}

/// What the frame's block cursor covers, shaped with the preparer's caches.
fn cursor_cell(
    frame: &Frame,
    metrics: CellMetrics,
    layouts: &mut LayoutCache,
    sprites: &mut sprite::SpriteCache,
    cost: &mut profile::Prepared,
) -> Option<CursorCell> {
    let cursor = frame.cursor;
    if !cursor.visible || cursor.shape != silo_vt::CursorShape::Block {
        return None;
    }
    cursor_cell_at(
        frame,
        (cursor.row, cursor.column),
        metrics,
        &mut |text, bold, italic| layouts.layout(text, bold, italic, metrics.font_size, cost),
        &mut |code, width| sprites.geometry(code, width, metrics.height, metrics.scale).map(sprite_content),
    )
}

/// The cell of `frame` at `(row, column)` (its wide character when that is
/// the spacer) as a block cursor covers it: laid out with `layout`
/// (text, bold, italic) or drawn with `sprite` (code point, width), without
/// color. The prepared frame's cursor uses the preparer's caches; a
/// predicted cursor is looked up while painting.
pub(crate) fn cursor_cell_at(
    frame: &Frame,
    (row, column): (u16, u16),
    metrics: CellMetrics,
    layout: &mut dyn FnMut(&str, bool, bool) -> Arc<LineLayout>,
    sprite: &mut dyn FnMut(char, f32) -> Option<CursorContent>,
) -> Option<CursorCell> {
    let columns = usize::from(frame.size.columns);
    let at = |column: u16| (column < frame.size.columns).then(|| frame.cells.get(usize::from(row) * columns + usize::from(column))).flatten();
    let mut column = column;
    if at(column)?.width == 0 && column > 0 && at(column - 1).is_some_and(|c| c.width == 2) {
        column -= 1;
    }
    let cell = at(column)?;
    let cells = u16::from(cell.width.max(1));
    let clip = cell_bounds(px(0.0), 0, usize::from(cells), metrics);
    let content = if cell.text.is_empty() || cell.width == 0 {
        CursorContent::Empty
    } else if let Some(code) = sprite::code_point(&cell.text) {
        sprite(code, f32::from(clip.size.width))?
    } else {
        let layout = layout(&cell.text, cell.bold, cell.italic);
        let inset = px(((f32::from(clip.size.width) - f32::from(layout.width)) / 2.0).max(0.0));
        CursorContent::Glyph(PreparedText { layout, origin: clip.origin, inset, starts: Vec::new(), colors: Vec::new() })
    };
    Some(CursorCell { row, column, cells, content })
}

/// A sprite's geometry for the cursor cell, relative to the cell.
pub(crate) fn sprite_content(shape: &sprite::Sprite) -> CursorContent {
    let quads = shape.quads.iter().map(|q| (Bounds::new(point(px(q.x), px(q.y)), size(px(q.width), px(q.height))), q.alpha)).collect();
    let polygons = shape.polygons.iter().map(|p| p.0.iter().map(|(x, y)| point(px(*x), px(*y))).collect()).collect();
    CursorContent::Sprite(quads, polygons)
}

/// A wide cell is shaped alone and centred in its two-cell box.
fn shape_single(
    layouts: &mut LayoutCache,
    cell: &Cell,
    clip: Bounds<Pixels>,
    metrics: CellMetrics,
    selection: Option<u32>,
    cost: &mut profile::Prepared,
) -> PreparedText {
    let color = text_color(cell_foreground(cell, selection), cell.faint);
    let layout = layouts.layout(&cell.text, cell.bold, cell.italic, metrics.font_size, cost);
    let inset = px(((f32::from(clip.size.width) - f32::from(layout.width)) / 2.0).max(0.0));
    PreparedText { layout, origin: clip.origin, inset, starts: Vec::new(), colors: vec![(cell.text.len(), color)] }
}

/// A run of narrow cells is shaped as one line; `starts` lets paint put
/// every cluster back on its own column.
fn shape_run(layouts: &mut LayoutCache, run: Run, origin: Point<Pixels>, metrics: CellMetrics, cost: &mut profile::Prepared) -> PreparedText {
    let mut colors = Vec::with_capacity(run.segments.len());
    let mut end = 0;
    for segment in &run.segments {
        end += segment.bytes;
        colors.push((end, text_color(segment.foreground, segment.faint)));
    }
    let layout = layouts.layout(&run.text, run.bold, run.italic, metrics.font_size, cost);
    PreparedText { layout, origin, inset: px(0.0), starts: run.starts, colors }
}

#[cfg(test)]
mod tests {
    use super::{Translucency, cell_alpha};

    #[test]
    fn cell_alpha_veils_only_backdrops() {
        let (default, tui, other) = (0x0028_2c34, 0x000f_2027, 0x00ff_0000);
        assert_eq!(cell_alpha(other, default, None), Some(255), "solid: opaque");
        assert_eq!(cell_alpha(default, default, None), Some(255));
        let t = Translucency { card: default, veiled: None, opaque_alpha: 230 };
        assert_eq!(cell_alpha(default, default, Some(t)), None, "the card's veil shows");
        assert_eq!(cell_alpha(other, default, Some(t)), Some(230), "structure stays crisp");
        assert_eq!(cell_alpha(tui, default, Some(t)), Some(230), "an app's backdrop stays opaque unless veiled");
        let tinted = Translucency { card: tui, veiled: Some(tui), ..t };
        assert_eq!(cell_alpha(tui, default, Some(tinted)), None, "tinted card of the app's colour");
        assert_eq!(cell_alpha(default, default, Some(tinted)), None, "default background on a tinted card");
        let theme_card = Translucency { veiled: Some(tui), ..t };
        assert_eq!(cell_alpha(tui, default, Some(theme_card)), None, "translucent app background on an untinted card");
    }
}
