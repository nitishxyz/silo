//! The composer's multi-line editor: wrapped text, a caret and selection,
//! IME through GPUI's input handler. ⏎ submits, ⇧⏎/⌥⏎ insert a newline;
//! it grows to `max_lines` and then scrolls.

use std::{ops::Range, time::Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    GlobalElementId, Hsla, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollWheelEvent, SharedString, Style,
    Task, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, div, fill, point, prelude::*, px, relative, size,
};

use crate::blink::Blink;

pub enum EditorEvent {
    Changed,
    Submit,
    Escape,
}

pub struct Editor {
    pub focus: FocusHandle,
    text: String,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    scroll: Pixels,
    layout: Option<Layout>,
    selecting: bool,
    blink: Blink,
    caret: bool,
    blink_task: Option<Task<()>>,
    /// Shown as dots (API keys).
    pub mask: bool,
}

struct Layout {
    lines: Vec<WrappedLine>,
    /// Byte offset of each hard line.
    starts: Vec<usize>,
    /// Top of each hard line, from the content top.
    tops: Vec<Pixels>,
    origin: Point<Pixels>,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
}

impl Layout {
    fn line_of(&self, offset: usize) -> usize {
        self.starts.iter().rposition(|s| *s <= offset).unwrap_or(0)
    }

    /// Caret position of `offset`, relative to the content top-left.
    fn position(&self, offset: usize) -> Point<Pixels> {
        let i = self.line_of(offset);
        let p = self.lines[i].position_for_index(offset - self.starts[i], self.line_height).unwrap_or_default();
        point(p.x, p.y + self.tops[i])
    }

    /// The offset nearest to `p` (relative to the content top-left).
    fn offset(&self, p: Point<Pixels>) -> usize {
        let i = self.tops.iter().rposition(|t| *t <= p.y).unwrap_or(0);
        let local = point(p.x.max(px(0.0)), p.y - self.tops[i]);
        let index = match self.lines[i].closest_index_for_position(local, self.line_height) {
            Ok(i) | Err(i) => i,
        };
        self.starts[i] + index.min(self.lines[i].len())
    }

    fn height(&self) -> Pixels {
        self.tops.last().copied().unwrap_or_default() + self.lines.last().map_or(self.line_height, |l| l.size(self.line_height).height)
    }
}

impl EventEmitter<EditorEvent> for Editor {}

impl Editor {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: String::new(),
            selection: 0..0,
            reversed: false,
            marked: None,
            placeholder: placeholder.into(),
            scroll: px(0.0),
            layout: None,
            selecting: false,
            blink: Blink::default(),
            caret: false,
            blink_task: None,
            mask: false,
        }
    }

    fn display(&self) -> String {
        if self.mask { "\u{2022}".repeat(self.text.chars().count()) } else { self.text.clone() }
    }

    /// Text offset → offset in the displayed text.
    fn disp(&self, offset: usize) -> usize {
        if self.mask { self.text[..offset.min(self.text.len())].chars().count() * '\u{2022}'.len_utf8() } else { offset }
    }

    /// Displayed offset → text offset.
    fn undisp(&self, offset: usize) -> usize {
        if self.mask { self.text.char_indices().nth(offset / '\u{2022}'.len_utf8()).map_or(self.text.len(), |(i, _)| i) } else { offset }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        let placeholder = placeholder.into();
        if self.placeholder != placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.selection = self.text.len()..self.text.len();
        self.marked = None;
        self.scroll = px(0.0);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed { self.selection.start } else { self.selection.end }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selection = offset..offset;
        self.reversed = false;
        self.blink.reset();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selection.start = offset;
        } else {
            self.selection.end = offset;
        }
        if self.selection.end < self.selection.start {
            self.reversed = !self.reversed;
            self.selection = self.selection.end..self.selection.start;
        }
        self.blink.reset();
        cx.notify();
    }

    fn go(&mut self, offset: usize, extend: bool, cx: &mut Context<Self>) {
        if extend { self.select_to(offset, cx) } else { self.move_to(offset, cx) }
    }

    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.text.replace_range(range.clone(), text);
        let end = range.start + text.len();
        self.selection = end..end;
        self.reversed = false;
        self.marked = None;
        self.blink.reset();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.marked.clone().unwrap_or(self.selection.clone());
        self.replace(range, text, cx);
    }

    fn delete(&mut self, to: usize, cx: &mut Context<Self>) {
        let range = if self.selection.is_empty() { to.min(self.cursor())..to.max(self.cursor()) } else { self.selection.clone() };
        if !range.is_empty() {
            self.replace(range, "", cx);
        }
    }

    fn selected(&self) -> Option<String> {
        (!self.selection.is_empty()).then(|| self.text[self.selection.clone()].to_owned())
    }

    fn vertical(&self, down: bool) -> usize {
        let Some(layout) = &self.layout else {
            return if down { self.text.len() } else { 0 };
        };
        let p = layout.position(self.disp(self.cursor()));
        let y = if down { p.y + layout.line_height * 1.5 } else { p.y - layout.line_height * 0.5 };
        if y < px(0.0) {
            0
        } else if y >= layout.height() {
            self.text.len()
        } else {
            self.undisp(layout.offset(point(p.x, y)))
        }
    }

    fn key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.marked.is_some() {
            return false;
        }
        let k = &event.keystroke;
        let m = k.modifiers;
        let cursor = self.cursor();
        match k.key.as_str() {
            "enter" if m.shift || m.alt => self.insert("\n", cx),
            "enter" if !m.platform && !m.control => cx.emit(EditorEvent::Submit),
            "escape" => cx.emit(EditorEvent::Escape),
            "backspace" => {
                let to = if m.platform {
                    line_start(&self.text, cursor)
                } else if m.alt {
                    word_start(&self.text, cursor)
                } else {
                    prev_char(&self.text, cursor)
                };
                self.delete(to, cx);
            }
            "delete" => {
                let to = if m.alt { word_end(&self.text, cursor) } else { next_char(&self.text, cursor) };
                self.delete(to, cx);
            }
            "left" => {
                let to = if m.platform {
                    line_start(&self.text, cursor)
                } else if m.alt {
                    word_start(&self.text, cursor)
                } else if !self.selection.is_empty() && !m.shift {
                    self.selection.start
                } else {
                    prev_char(&self.text, cursor)
                };
                self.go(to, m.shift, cx);
            }
            "right" => {
                let to = if m.platform {
                    line_end(&self.text, cursor)
                } else if m.alt {
                    word_end(&self.text, cursor)
                } else if !self.selection.is_empty() && !m.shift {
                    self.selection.end
                } else {
                    next_char(&self.text, cursor)
                };
                self.go(to, m.shift, cx);
            }
            "up" if m.platform => self.go(0, m.shift, cx),
            "down" if m.platform => self.go(self.text.len(), m.shift, cx),
            "up" => self.go(self.vertical(false), m.shift, cx),
            "down" => self.go(self.vertical(true), m.shift, cx),
            "home" => self.go(line_start(&self.text, cursor), m.shift, cx),
            "end" => self.go(line_end(&self.text, cursor), m.shift, cx),
            "x" if m.platform => {
                if let Some(text) = self.selected() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    let range = self.selection.clone();
                    self.replace(range, "", cx);
                }
            }
            "a" if m.control => self.go(line_start(&self.text, cursor), m.shift, cx),
            "e" if m.control => self.go(line_end(&self.text, cursor), m.shift, cx),
            "k" if m.control => {
                let end = line_end(&self.text, cursor);
                let end = if end == cursor { next_char(&self.text, cursor) } else { end };
                self.delete(end, cx);
            }
            _ => return false,
        }
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.insert(&text.replace("\r\n", "\n"), cx);
        }
    }

    fn caret_phase(&mut self, shown: bool, cx: &mut Context<Self>) -> bool {
        if shown && !self.caret {
            self.blink.reset();
        }
        self.caret = shown;
        if !shown {
            return false;
        }
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    let wait = this.update(cx, |e, _| {
                        if e.caret {
                            Some(e.blink.until_next(Instant::now()))
                        } else {
                            e.blink_task = None;
                            None
                        }
                    });
                    let Ok(Some(wait)) = wait else { return };
                    cx.background_executor().timer(wait).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }));
        }
        self.blink.visible_at(Instant::now())
    }

    fn utf16_to_offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (i, ch) in self.text.char_indices() {
            if units >= utf16 {
                return i;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())].encode_utf16().count()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.utf16_to_offset(r.start)..self.utf16_to_offset(r.end)
    }

    fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end)
    }

    /// The editor element with its interactions; `max_lines` before it scrolls.
    pub fn element(entity: &Entity<Self>, max_lines: usize, colors: EditorColors, cx: &mut App) -> impl IntoElement + use<> {
        let focus = entity.read(cx).focus.clone();
        let e = entity.clone();
        let wheel = entity.clone();
        let mover = entity.clone();
        div()
            .id("agent-editor")
            .w_full()
            .cursor_text()
            .track_focus(&focus)
            .key_context("AgentEditor")
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if e.update(cx, |ed, cx| ed.key(event, cx)) {
                    cx.stop_propagation();
                }
            })
            .on_action({
                let e = entity.clone();
                move |_: &crate::commands::act::Paste, _, cx| e.update(cx, |ed, cx| ed.paste(cx))
            })
            .on_action({
                let e = entity.clone();
                move |_: &crate::commands::act::SelectAll, _, cx| {
                    e.update(cx, |ed, cx| {
                        ed.selection = 0..ed.text.len();
                        ed.reversed = false;
                        cx.notify();
                    });
                }
            })
            .on_action({
                let e = entity.clone();
                move |_: &crate::commands::act::Copy, _, cx| {
                    if let Some(text) = e.read(cx).selected() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let e = entity.clone();
                move |event: &MouseDownEvent, window, cx| {
                    let focus = e.read(cx).focus.clone();
                    window.focus(&focus, cx);
                    e.update(cx, |ed, cx| {
                        let Some(layout) = &ed.layout else { return };
                        let offset = ed.undisp(layout.offset(event.position - layout.origin));
                        ed.selecting = true;
                        if event.click_count >= 2 {
                            ed.selection = word_start(&ed.text, offset)..word_end(&ed.text, offset);
                            ed.reversed = false;
                            cx.notify();
                        } else {
                            ed.go(offset, event.modifiers.shift, cx);
                        }
                    });
                }
            })
            .on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
                mover.update(cx, |ed, cx| {
                    if !ed.selecting || event.pressed_button != Some(MouseButton::Left) {
                        ed.selecting = false;
                        return;
                    }
                    if let Some(layout) = &ed.layout {
                        let offset = ed.undisp(layout.offset(event.position - layout.origin));
                        ed.select_to(offset, cx);
                    }
                });
            })
            .on_mouse_up(MouseButton::Left, {
                let e = entity.clone();
                move |_, _, cx| e.update(cx, |ed, _| ed.selecting = false)
            })
            .on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
                wheel.update(cx, |ed, cx| {
                    let Some(layout) = &ed.layout else { return };
                    let max = (layout.height() - layout.bounds.size.height).max(px(0.0));
                    if max > px(0.0) {
                        let dy = event.delta.pixel_delta(window.line_height()).y;
                        ed.scroll = (ed.scroll - dy).clamp(px(0.0), max);
                        cx.notify();
                        cx.stop_propagation();
                    }
                });
            })
            .child(EditorElement { editor: entity.clone(), max_lines, colors })
    }
}

#[derive(Clone, Copy)]
pub struct EditorColors {
    pub text: Hsla,
    pub placeholder: Hsla,
    pub caret: Hsla,
    pub selection: Hsla,
}

struct EditorElement {
    editor: Entity<Editor>,
    max_lines: usize,
    colors: EditorColors,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn runs(text: &str, marked: Option<&Range<usize>>, window: &Window, color: Hsla) -> Vec<TextRun> {
    let run = TextRun { len: text.len(), font: window.text_style().font(), color, background_color: None, underline: None, strikethrough: None };
    match marked {
        Some(m) if m.end <= text.len() => [
            TextRun { len: m.start, ..run.clone() },
            TextRun { len: m.end - m.start, underline: Some(UnderlineStyle { color: Some(color), thickness: px(1.0), wavy: false }), ..run.clone() },
            TextRun { len: text.len() - m.end, ..run },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect(),
        _ => vec![run],
    }
}

fn shape(text: &str, runs: &[TextRun], width: Pixels, window: &Window) -> Vec<WrappedLine> {
    let size = window.text_style().font_size.to_pixels(window.rem_size());
    window.text_system().shape_text(SharedString::from(text.to_owned()), size, runs, Some(width), None).map(|l| l.into_vec()).unwrap_or_default()
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let editor = self.editor.read(cx);
        let text = if editor.text.is_empty() { editor.placeholder.to_string() } else { editor.display() };
        let max = self.max_lines;
        let color = self.colors.text;
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let lh = window.line_height();
            let lines = width.map_or(1, |w| {
                let runs = runs(&text, None, window, color);
                shape(&text, &runs, w, window).iter().map(|l| l.wrap_boundaries().len() + 1).sum::<usize>().max(1)
            });
            #[allow(clippy::cast_precision_loss)]
            size(width.unwrap_or(px(0.0)), lh * lines.min(max) as f32)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let (text, marked, empty, focus) = {
            let e = self.editor.read(cx);
            let empty = e.text.is_empty();
            let marked = e.marked.as_ref().map(|m| e.disp(m.start)..e.disp(m.end));
            (if empty { e.placeholder.to_string() } else { e.display() }, marked, empty, e.focus.clone())
        };
        let color = if empty { self.colors.placeholder } else { self.colors.text };
        let runs = runs(&text, marked.as_ref(), window, color);
        let lines = shape(&text, &runs, bounds.size.width, window);
        let line_height = window.line_height();
        let mut starts = Vec::with_capacity(lines.len());
        let mut tops = Vec::with_capacity(lines.len());
        let (mut offset, mut top) = (0, px(0.0));
        for line in &lines {
            starts.push(offset);
            tops.push(top);
            offset += line.len() + 1;
            top += line.size(line_height).height;
        }
        if lines.is_empty() {
            return;
        }
        let focused = focus.is_focused(window) && window.is_window_active();
        let mut layout = Layout { lines, starts, tops, origin: bounds.origin, line_height, bounds };
        let (selection, scroll, show_caret) = self.editor.update(cx, |e, cx| {
            let caret = if empty { point(px(0.0), px(0.0)) } else { layout.position(e.disp(e.cursor())) };
            let max = (layout.height() - bounds.size.height).max(px(0.0));
            if caret.y < e.scroll {
                e.scroll = caret.y;
            } else if caret.y + line_height > e.scroll + bounds.size.height {
                e.scroll = caret.y + line_height - bounds.size.height;
            }
            e.scroll = e.scroll.clamp(px(0.0), max);
            let show = e.caret_phase(focused && e.selection.is_empty(), cx);
            (if empty { 0..0 } else { e.disp(e.selection.start)..e.disp(e.selection.end) }, e.scroll, show)
        });
        layout.origin = point(bounds.origin.x, bounds.origin.y - scroll);
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.editor.clone()), cx);
        window.with_content_mask(Some(gpui::ContentMask { bounds, ..Default::default() }), |window| {
            if !selection.is_empty() {
                let (a, b) = (layout.position(selection.start), layout.position(selection.end));
                let row = |y: Pixels, x0: Pixels, x1: Pixels| {
                    fill(Bounds::from_corners(layout.origin + point(x0, y), layout.origin + point(x1, y + line_height)), self.colors.selection)
                };
                let right = bounds.size.width;
                if a.y == b.y {
                    window.paint_quad(row(a.y, a.x, b.x));
                } else {
                    window.paint_quad(row(a.y, a.x, right));
                    let mut y = a.y + line_height;
                    while y < b.y {
                        window.paint_quad(row(y, px(0.0), right));
                        y += line_height;
                    }
                    window.paint_quad(row(b.y, px(0.0), b.x));
                }
            }
            for (line, top) in layout.lines.iter().zip(&layout.tops) {
                let origin = layout.origin + point(px(0.0), *top);
                if origin.y > bounds.bottom() || origin.y + line.size(line_height).height < bounds.top() {
                    continue;
                }
                let _ = line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx);
            }
            if show_caret {
                let cursor = {
                    let e = self.editor.read(cx);
                    e.disp(e.cursor())
                };
                let p = if empty { point(px(0.0), px(0.0)) } else { layout.position(cursor) };
                window.paint_quad(fill(
                    Bounds::new(layout.origin + p + point(px(-0.5), px(2.0)), size(px(1.5), line_height - px(4.0))),
                    self.colors.caret,
                ));
            }
        });
        let changed = self.editor.read(cx).layout.as_ref().is_none_or(|old| old.origin != layout.origin);
        self.editor.update(cx, |e, _| e.layout = Some(layout));
        if changed {
            window.invalidate_character_coordinates();
        }
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let range = self.range_from_utf16(&range);
        adjusted.replace(self.range_to_utf16(&range));
        Some(self.text[range].to_owned())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selection), reversed: self.reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.text.encode_utf16().count())
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = range.as_ref().map(|r| self.range_from_utf16(r)).or(self.marked.clone()).unwrap_or(self.selection.clone());
        self.replace(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.as_ref().map(|r| self.range_from_utf16(r)).or(self.marked.clone()).unwrap_or(self.selection.clone());
        self.text.replace_range(range.clone(), text);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        self.selection = match selected {
            Some(sel) => {
                let units: Vec<u16> = text.encode_utf16().collect();
                let byte = |u: usize| String::from_utf16_lossy(&units[..u.min(units.len())]).len();
                range.start + byte(sel.start)..range.start + byte(sel.end)
            }
            None => range.start + text.len()..range.start + text.len(),
        };
        self.reversed = false;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn set_selected_text_range(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = self.range_from_utf16(&range);
        self.reversed = false;
        cx.notify();
    }

    fn bounds_for_range(&mut self, range: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = self.range_from_utf16(&range);
        let start = layout.position(self.disp(range.start));
        let end = layout.position(self.disp(range.end));
        let end_x = if end.y == start.y { end.x } else { start.x + px(1.0) };
        Some(Bounds::from_corners(layout.origin + start, layout.origin + point(end_x.max(start.x + px(1.0)), start.y + layout.line_height)))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        Some(self.offset_to_utf16(self.undisp(layout.offset(p - layout.origin))))
    }
}

// ---- caret motion over text ----

fn prev_char(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_start(text: &str, at: usize) -> usize {
    let before = &text[..at];
    let trimmed = before.trim_end_matches(|c: char| !is_word(c));
    trimmed.char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(trimmed.len(), |(i, _)| i)
}

fn word_end(text: &str, at: usize) -> usize {
    let after = &text[at..];
    let skip = after.len() - after.trim_start_matches(|c: char| !is_word(c)).len();
    let rest = &after[skip..];
    at + skip + rest.find(|c: char| !is_word(c)).unwrap_or(rest.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_motion() {
        let t = "fix the lock\nfile now";
        assert_eq!(word_start(t, 12), 8);
        assert_eq!(word_start(t, 8), 4);
        assert_eq!(word_end(t, 0), 3);
        assert_eq!(word_end(t, 3), 7);
        assert_eq!(line_start(t, 15), 13);
        assert_eq!(line_end(t, 2), 12);
        assert_eq!(line_end(t, 15), t.len());
        let u = "aé😀";
        assert_eq!(prev_char(u, u.len()), 3);
        assert_eq!(next_char(u, 1), 3);
        assert_eq!(next_char(u, u.len()), u.len());
    }
}
