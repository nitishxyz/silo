//! The text viewer: a line-number gutter, lines coloured by `crate::syntax`,
//! the selection, find matches and an agent's highlights; pointer
//! selection like the agent pane's (press, drag, double-click a word,
//! triple-click a line, ⇧-click extends, past an edge scrolls). When the
//! tab is editable (`editor.rs`) it draws the caret and the input method's
//! marked text, and takes text input.

use std::ops::Range;

use gpui::{
    AnyElement, Context, DispatchPhase, ElementInputHandler, HighlightStyle, Hsla, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ScrollStrategy, SharedString, StyledText, UnderlineStyle, Window, div, prelude::*, px, rgb,
};

use super::{
    Content, Drag, FilesPane, TextFile, marks_of,
    model::{self, Pos, Selection, Unit},
};
use crate::theme;

/// Space left of the text, after the gutter.
pub const PAD: f32 = 10.0;
/// Characters of a line drawn; the rest is cut (minified files).
const DRAWN_MAX: usize = 4000;

/// The viewer's font measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub size: f32,
    pub line: f32,
    /// A character's advance (the font is monospace).
    pub cell: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { size: 12.5, line: 19.0, cell: 7.5 }
    }
}

/// The gutter's width for `lines` lines.
#[must_use]
pub fn gutter_width(lines: usize, cell: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let digits = lines.to_string().len().max(3) as f32;
    digits * cell + 22.0
}

/// The line at window `y` (clamped into the text): `top` is the text
/// area's top, `offset` its scroll (0 or less).
#[must_use]
pub fn line_at(y: f32, top: f32, offset: f32, line_height: f32, lines: usize) -> usize {
    let at = ((y - top - offset) / line_height).floor();
    if at < 0.0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let at = at as usize;
        at.min(lines.saturating_sub(1))
    }
}

/// How fast a drag at `y` scrolls (points per tick; negative: up) when
/// past the text area's `top` or `bottom`.
#[must_use]
pub fn edge_speed(y: f32, top: f32, bottom: f32) -> f32 {
    if y < top {
        -((top - y) / 2.0).clamp(2.0, 40.0)
    } else if y > bottom {
        ((y - bottom) / 2.0).clamp(2.0, 40.0)
    } else {
        0.0
    }
}

/// `syntax` (sorted, not overlapping) with `backgrounds` laid over it, the
/// later ones winning, as one list of styles for `StyledText`.
#[must_use]
pub fn combine(syntax: &[(Range<usize>, HighlightStyle)], backgrounds: &[(Range<usize>, Hsla)]) -> Vec<(Range<usize>, HighlightStyle)> {
    if backgrounds.is_empty() {
        return syntax.to_vec();
    }
    let mut cuts: Vec<usize> =
        syntax.iter().flat_map(|(r, _)| [r.start, r.end]).chain(backgrounds.iter().flat_map(|(r, _)| [r.start, r.end])).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a == b {
            continue;
        }
        let mut style = syntax.iter().find(|(r, _)| r.start <= a && b <= r.end).map(|(_, s)| *s).unwrap_or_default();
        if let Some((_, bg)) = backgrounds.iter().rfind(|(r, _)| r.start <= a && b <= r.end) {
            style.background_color = Some(*bg);
        }
        if style == HighlightStyle::default() {
            continue;
        }
        match out.last_mut() {
            Some((r, s)) if r.end == a && *s == style => r.end = b,
            _ => out.push((a..b, style)),
        }
    }
    out
}

/// `spans` with bytes `marked` underlined (an input method's marked text).
#[must_use]
pub fn underline(spans: &[(Range<usize>, HighlightStyle)], marked: &Range<usize>, color: Hsla) -> Vec<(Range<usize>, HighlightStyle)> {
    if marked.is_empty() {
        return spans.to_vec();
    }
    let mut cuts: Vec<usize> = spans.iter().flat_map(|(r, _)| [r.start, r.end]).chain([marked.start, marked.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let line = UnderlineStyle { thickness: px(1.0), color: Some(color), wavy: false };
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut style = spans.iter().find(|(r, _)| r.start <= a && b <= r.end).map(|(_, s)| *s).unwrap_or_default();
        if marked.start <= a && b <= marked.end {
            style.underline = Some(line);
        }
        if style != HighlightStyle::default() {
            out.push((a..b, style));
        }
    }
    out
}

fn alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

/// The selection's colour (the agent pane's).
fn selection_bg() -> Hsla {
    alpha(theme::selection(), 0.6)
}

impl FilesPane {
    /// The font measures for this window, kept for hit-testing.
    pub(crate) fn measure(&self, window: &Window) -> Metrics {
        let (_, family, size) = theme::summary();
        let size = size.clamp(11.0, 16.0);
        let font = gpui::font(SharedString::from(family));
        let id = window.text_system().resolve_font(&font);
        let cell = window.text_system().advance(id, px(size), 'm').map_or(size * 0.6, |a| f32::from(a.width));
        let m = Metrics { size, line: (size * 1.5).round(), cell };
        self.metrics.set(m);
        m
    }

    /// The text viewer for `path`.
    pub(crate) fn render_text(&mut self, path: &str, file: &TextFile, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let m = self.measure(window);
        let (_, family, _) = theme::summary();
        let family = SharedString::from(family);
        let doc = file.doc.clone();
        let handle = self.scroll_handle(path);
        if let Some(top) = self.wanted_top(path) {
            #[allow(clippy::cast_possible_truncation)]
            handle.scroll_to_item_strict(top as usize, ScrollStrategy::Top);
        }
        let lines = doc.line_count();
        let gutter = gutter_width(lines, m.cell);
        let marks = marks_of(&self.state, path).to_vec();
        let syntax = file.syntax.clone();
        // Editable: a caret (at the selection's head) while the viewer has focus, and text input.
        let editable = self.writes() && file.edit.is_some();
        let focused = self.focus.is_focused(window) && window.is_window_active();
        let selection = if editable { Some(self.edit_selection(&doc)) } else { self.selection };
        let blink = self.caret_phase(editable && focused, cx);
        let caret = selection.filter(|_| blink).map(|s| {
            let head = doc.clamp(s.head);
            let line = doc.line(head.line);
            #[allow(clippy::cast_precision_loss)]
            let x = model::columns(&line[..head.col]) as f32 * m.cell;
            (head.line, x)
        });
        let marked = file.edit.as_ref().and_then(|e| e.marked.clone()).filter(|_| editable).map(|r| {
            let (a, b) = (doc.pos(r.start), doc.pos(r.end));
            (a.line, a.col..if b.line == a.line { b.col } else { doc.line(a.line).len() })
        });
        let (caret_color, ink) = (rgb(theme::accent()), rgb(theme::foreground()).into());
        let find: Vec<(usize, Range<usize>)> = self.find.as_ref().map(|f| f.matches.clone()).unwrap_or_default();
        let current = self.find.as_ref().and_then(|f| f.current).map(|i| find[i].clone());
        let bg = theme::background();
        let (find_bg, current_bg) = (alpha(theme::warning(), 0.28), alpha(theme::warning(), 0.6));
        let row_doc = doc.clone();
        let rows = gpui::uniform_list(gpui::ElementId::Name(format!("files-text-{path}").into()), lines, move |range, _, _| {
            range
                .map(|ix| {
                    let line = row_doc.line(ix);
                    let cut = line.char_indices().nth(DRAWN_MAX).map_or(line.len(), |(i, _)| i);
                    let src = &line[..cut];
                    let mut shown = model::expand_tabs(src).into_owned();
                    let spans = syntax.as_ref().and_then(|s| s.get(ix)).map(|spans| crate::syntax::styles(spans, bg)).unwrap_or_default();
                    let to_shown = |r: &Range<usize>| model::to_shown(src, r.start.min(cut))..model::to_shown(src, r.end.min(cut));
                    let spans: Vec<_> = spans.iter().map(|(r, s)| (to_shown(r), *s)).filter(|(r, _)| r.start < r.end).collect();
                    let mut backs: Vec<(Range<usize>, Hsla)> = find.iter().filter(|(l, _)| *l == ix).map(|(_, r)| (to_shown(r), find_bg)).collect();
                    if let Some((_, r)) = current.as_ref().filter(|(l, _)| *l == ix) {
                        backs.push((to_shown(r), current_bg));
                    }
                    if let Some(r) = selection.as_ref().and_then(|s| s.on_line(&row_doc, ix)) {
                        // The line break selected: a space after the text shows it.
                        let past = r.end > line.len();
                        if past {
                            shown.push(' ');
                        }
                        let end = if past { shown.len() } else { model::to_shown(src, r.end.min(cut)) };
                        backs.push((model::to_shown(src, r.start.min(cut))..end, selection_bg()));
                    }
                    if shown.is_empty() {
                        shown.push(' ');
                    }
                    let mut styles = combine(&spans, &backs);
                    if let Some((_, r)) = marked.as_ref().filter(|(l, _)| *l == ix) {
                        styles = underline(&styles, &(model::to_shown(src, r.start.min(cut))..model::to_shown(src, r.end.min(cut))), ink);
                    }
                    let styled = StyledText::new(shown).with_highlights(styles);
                    let caret = caret
                        .filter(|(l, _)| *l == ix)
                        .map(|(_, x)| div().absolute().top(px(2.0)).left(px(gutter + PAD + x - 0.5)).w(px(1.5)).h(px(m.line - 4.0)).bg(caret_color));
                    div()
                        .relative()
                        .h(px(m.line))
                        .pl(px(gutter + PAD))
                        .pr(px(PAD * 2.0))
                        .flex_none()
                        .whitespace_nowrap()
                        .child(styled)
                        .children(caret)
                })
                .collect()
        })
        .track_scroll(&handle)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(doc.widest))
        .size_full()
        .font_family(family.clone())
        .text_size(px(m.size))
        .line_height(px(m.line))
        .text_color(rgb(theme::foreground()))
        .cursor_text();

        // The gutter, the highlights' tint and their notes are drawn from the
        // scroll offset (the list moves its rows itself): what shows of them.
        let offset = handle.0.borrow().base_handle.offset();
        let (ox, oy) = (f32::from(offset.x), f32::from(offset.y));
        let height = self.text_bounds.get().map_or(800.0, |b| f32::from(b.size.height));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let first = (-oy / m.line).floor().max(0.0) as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let last = (first + (height / m.line).ceil() as usize + 1).min(lines);
        #[allow(clippy::cast_precision_loss)]
        let y_of = |ix: usize| ix as f32 * m.line + oy;
        let muted = theme::muted();
        let accent = theme::accent();
        let numbers = (first..last).map(|ix| {
            let mark = model::mark_at(&marks, ix as u64 + 1);
            div()
                .absolute()
                .top(px(y_of(ix)))
                .left_0()
                .w(px(gutter))
                .h(px(m.line))
                .pr(px(10.0))
                .flex()
                .justify_end()
                .text_color(rgb(if mark.is_some() { accent } else { muted }))
                .child(SharedString::from((ix + 1).to_string()))
                .children(mark.map(|_| div().absolute().right_0().top_0().w(px(2.5)).h_full().bg(rgb(accent))))
        });
        let tints = (first..last)
            .filter(|&ix| model::mark_at(&marks, ix as u64 + 1).is_some())
            .map(|ix| div().absolute().top(px(y_of(ix))).left_0().right_0().h(px(m.line)).bg(alpha(accent, 0.13)));
        let notes = (first..last).filter_map(|ix| {
            let (i, start) = model::mark_at(&marks, ix as u64 + 1)?;
            let note = marks[i].note.clone().filter(|_| start || ix == first)?;
            Some(
                div()
                    .absolute()
                    .top(px(y_of(ix) + 1.0))
                    .right(px(12.0))
                    .h(px(m.line - 2.0))
                    .max_w(px(360.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .bg(rgb(theme::mix(bg, accent, 0.22)))
                    .border_1()
                    .border_color(alpha(accent, 0.5))
                    .text_color(rgb(theme::foreground()))
                    .text_size(px(11.5))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(note),
            )
        });
        let bounds = self.text_bounds.clone();
        let used = offset;
        let probe_handle = handle.clone();
        let pane = cx.entity().downgrade();
        let input = editable.then(|| (self.focus.clone(), cx.entity()));
        // After the list in paint order: its bounds, its final offset (a scroll to a line lands in its
        // prepaint: draw again so the gutter follows), and pointer moves and releases anywhere.
        let probe = gpui::canvas(
            move |b, _, _| b,
            move |b, _, window, cx| {
                bounds.set(Some(b));
                if probe_handle.0.borrow().base_handle.offset() != used {
                    window.refresh();
                }
                // Typing and input methods (marked text) reach the pane while it has focus.
                if let Some((focus, pane)) = input {
                    window.handle_input(&focus, ElementInputHandler::new(b, pane), cx);
                }
                let moves = pane.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = moves.update(cx, |this, cx| this.text_move(e, window, cx));
                    }
                });
                let ups = pane.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        let _ = ups.update(cx, |this, cx| this.text_up(cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        if let Some(top) = self.top_line(path) {
            self.scrolled(path, top, cx);
        }
        let _ = ox;
        div()
            .id("files-text")
            .relative()
            .size_full()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, window, cx| this.text_down(e, window, cx)))
            .children(tints)
            .child(rows)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(gutter))
                    .h_full()
                    .font_family(family)
                    .text_size(px(m.size))
                    .line_height(px(m.line))
                    .children(numbers),
            )
            .children(notes)
            .child(probe)
            .into_any_element()
    }

    /// The text position at window point `p`.
    pub(super) fn pos_at(&self, p: gpui::Point<gpui::Pixels>) -> Option<Pos> {
        let doc = self.shown_doc()?;
        let bounds = self.text_bounds.get()?;
        let path = self.state.active.as_ref()?;
        let offset = self.scrolls.get(path)?.0.borrow().base_handle.offset();
        let m = self.metrics.get();
        let line = line_at(f32::from(p.y), f32::from(bounds.top()), f32::from(offset.y), m.line, doc.line_count());
        let x = f32::from(p.x) - f32::from(bounds.left()) - f32::from(offset.x) - gutter_width(doc.line_count(), m.cell) - PAD;
        Some(Pos { line, col: model::col_at(doc.line(line), x / m.cell) })
    }

    fn text_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(at) = self.pos_at(e.position) else { return };
        let unit = match e.click_count {
            0 | 1 => Unit::Char,
            2 => Unit::Word,
            _ => Unit::Line,
        };
        let editable = self.editing();
        self.selection = match self.selection {
            Some(mut sel) if e.modifiers.shift => {
                sel.head = at;
                Some(sel)
            }
            // Editing, ⇧-click extends from the caret even before a selection.
            None if e.modifiers.shift && editable => self.shown_doc().map(|doc| Selection { head: at, ..self.edit_selection(&doc) }),
            _ => Some(Selection::new(at, unit)),
        };
        if let Some(Content::Text(file)) = self.state.active.as_ref().and_then(|a| self.contents.get_mut(a))
            && let Some(state) = file.edit.as_mut()
        {
            // A click ends an undo step and any composing.
            state.history.seal();
            state.goal = None;
            state.marked = None;
        }
        self.blink.reset();
        self.drag = Some(Drag::Select);
        cx.notify();
    }

    pub(crate) fn text_move(&mut self, e: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Select) => {}
            Some(Drag::Tree { width, x }) => {
                if e.pressed_button != Some(MouseButton::Left) {
                    return self.text_up(cx);
                }
                let max = self.body_bounds.get().map_or(super::TREE_MAX, |b| (f32::from(b.size.width) * 0.7).min(super::TREE_MAX));
                self.tree_width = (width + x - f32::from(e.position.x)).clamp(super::TREE_MIN, max.max(super::TREE_MIN));
                cx.notify();
                return;
            }
            None => return,
        }
        if e.pressed_button != Some(MouseButton::Left) {
            return self.text_up(cx);
        }
        let _ = window;
        self.autoscroll_speed =
            self.text_bounds.get().map_or(0.0, |b| edge_speed(f32::from(e.position.y), f32::from(b.top()), f32::from(b.bottom())));
        if let Some(at) = self.pos_at(e.position)
            && let Some(sel) = self.selection.as_mut()
            && sel.head != at
        {
            sel.head = at;
            cx.notify();
        }
        if self.autoscroll_speed != 0.0 {
            self.run_autoscroll(cx);
        }
    }

    pub(crate) fn text_up(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.autoscroll_speed = 0.0;
            cx.notify();
        }
    }

    /// A key in the viewer: arrows and page keys scroll.
    pub(crate) fn text_key(&mut self, key: &str, platform: bool) -> bool {
        let Some(path) = self.state.active.clone() else { return false };
        let Some(handle) = self.scrolls.get(&path) else { return false };
        let base = handle.0.borrow().base_handle.clone();
        let (offset, max) = (base.offset(), base.max_offset());
        let m = self.metrics.get();
        let page = self.text_bounds.get().map_or(400.0, |b| f32::from(b.size.height)) - m.line * 2.0;
        let (x, y) = (f32::from(offset.x), f32::from(offset.y));
        let (x, y) = match (key, platform) {
            ("up", true) | ("home", _) => (x, 0.0),
            ("down", true) | ("end", _) => (x, -f32::from(max.y)),
            ("up", false) => (x, y + m.line * 3.0),
            ("down", false) => (x, y - m.line * 3.0),
            ("pageup", _) => (x, y + page),
            ("pagedown", _) | ("space", false) => (x, y - page),
            ("left", false) => (x + m.cell * 8.0, y),
            ("right", false) => (x - m.cell * 8.0, y),
            _ => return false,
        };
        base.set_offset(gpui::point(px(x.clamp(-f32::from(max.x), 0.0)), px(y.clamp(-f32::from(max.y), 0.0))));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_edges_from_the_pointer() {
        // 20pt lines, text area from y=100, scrolled 50pt down.
        assert_eq!(line_at(100.0, 100.0, -50.0, 20.0, 100), 2);
        assert_eq!(line_at(139.0, 100.0, 0.0, 20.0, 100), 1);
        assert_eq!(line_at(10.0, 100.0, 0.0, 20.0, 100), 0, "above: the first line");
        assert_eq!(line_at(9000.0, 100.0, 0.0, 20.0, 100), 99, "below: the last");
        assert!(edge_speed(90.0, 100.0, 500.0) < 0.0 && edge_speed(600.0, 100.0, 500.0) > 0.0);
        assert!(edge_speed(300.0, 100.0, 500.0) == 0.0);
        assert!((gutter_width(5, 8.0) - (3.0 * 8.0 + 22.0)).abs() < 0.01, "room for three digits at least");
        assert!((gutter_width(12_345, 8.0) - (5.0 * 8.0 + 22.0)).abs() < 0.01);
    }

    #[test]
    fn backgrounds_lay_over_syntax_colours() {
        let red = HighlightStyle { color: Some(gpui::red()), ..HighlightStyle::default() };
        let (find, sel) = (gpui::yellow(), gpui::blue());
        let out = combine(&[(0..6, red)], &[(4..10, find), (5..8, sel)]);
        let bg = |c| HighlightStyle { background_color: Some(c), ..HighlightStyle::default() };
        let red_on = |c| HighlightStyle { background_color: Some(c), ..red };
        assert_eq!(out, vec![(0..4, red), (4..5, red_on(find)), (5..6, red_on(sel)), (6..8, bg(sel)), (8..10, bg(find))]);
        assert_eq!(combine(&[(0..3, red)], &[]), vec![(0..3, red)]);
    }
}
