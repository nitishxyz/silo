//! Drawing the Changes block: its header (scope picker, Unified/Split, the
//! summary and file tabs, totals, the list toggle), the diff (rows from
//! [`model::rows`]), the file list, the comment bar and the commit footer.

use std::ops::Range;

use gpui::{
    AnyElement, ClickEvent, Context, Hsla, IntoElement, KeyDownEvent, MouseButton, Render, SharedString, StyledText, Window, div, prelude::*, px, rgb,
};
use silo_proto::model::{BlockInfo, Side};

use super::{ChangesPane, HunkAction, Loaded, model, scope_label, wire::LineKind};
use crate::{
    agent::editor::{Editor, EditorColors},
    commands::act,
    files::model::{expand_tabs, tab_labels, to_shown},
    pane_header::{BUTTON, BlockHeader, HeaderParts, HeaderStyle, button, keep_press, labels},
    theme::{self, Continuous as _},
};

/// A list row's height.
const ROW: f32 = 24.0;
/// Space around the diff's text.
const PAD: f32 = 10.0;

/// The diff's font measures.
#[derive(Clone, Copy)]
struct Metrics {
    size: f32,
    line: f32,
    cell: f32,
}

/// A hunk bar's text: git's header (the enclosing function), else where
/// the hunk is, so a new file's bar isn't blank.
pub(super) fn hunk_title(h: &silo_proto::changes::Hunk) -> SharedString {
    let header = h.header.trim();
    if !header.is_empty() {
        return header.to_owned().into();
    }
    let (start, lines) = if h.new_lines > 0 { (h.new_start, h.new_lines) } else { (h.old_start, h.old_lines) };
    match lines {
        0 => "Empty".into(),
        1 => format!("Line {start}").into(),
        n => format!("Lines {start}\u{2013}{}", start + n - 1).into(),
    }
}

/// The sideways scroll `x`, kept between the start and where the longest
/// line (`widest`) ends at the right of a `view` wide code column.
pub(super) fn sideways(x: f32, widest: f32, view: f32) -> f32 {
    x.clamp(0.0, (widest - view + 24.0).max(0.0))
}

fn alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

fn hint(text: impl Into<SharedString>) -> gpui::Div {
    div().text_color(rgb(theme::muted())).child(text.into())
}

fn centered(child: impl IntoElement) -> AnyElement {
    div().size_full().flex().flex_col().items_center().justify_center().gap(px(6.0)).p(px(24.0)).child(child).into_any_element()
}

/// The tints: an added line's, a removed line's, and their changed words'.
#[derive(Clone, Copy)]
struct Tints {
    added: u32,
    removed: u32,
    added_word: Hsla,
    removed_word: Hsla,
    empty: u32,
    hunk: u32,
}

impl Tints {
    fn now() -> Self {
        let bg = theme::background();
        Self {
            added: theme::mix(bg, theme::success(), 0.12),
            removed: theme::mix(bg, theme::danger(), 0.12),
            added_word: alpha(theme::success(), 0.32),
            removed_word: alpha(theme::danger(), 0.32),
            empty: theme::mix(bg, theme::muted(), 0.05),
            hunk: theme::mix(bg, theme::accent(), 0.07),
        }
    }

    fn line(self, kind: LineKind) -> Option<u32> {
        match kind {
            LineKind::Added => Some(self.added),
            LineKind::Removed => Some(self.removed),
            LineKind::Context | LineKind::NoNewline | LineKind::Unknown => None,
        }
    }
}

impl BlockHeader for ChangesPane {
    fn header(&mut self, style: HeaderStyle, _: Option<&BlockInfo>, cx: &mut Context<Self>) -> HeaderParts {
        let root = self.state.root.clone();
        let name = crate::files::model::name(&root).to_owned();
        let mut middle = div().flex().flex_row().items_center().gap(px(6.0)).flex_1().min_w_0().overflow_hidden();
        middle = middle.child(self.scope_button(style, cx)).child(self.split_switch(style, cx));
        if self.state.open.is_empty() {
            middle = middle.child(labels(name.clone(), None, style).middle);
        } else {
            middle = middle.child(self.summary_tab(style, cx)).child(self.tabs(style, cx));
        }
        let mut parts = HeaderParts::new(middle.into_any_element(), if name.is_empty() { "Changes".to_owned() } else { format!("Changes · {name}") });
        let mut actions = Vec::new();
        if let Some(status) = self.status.as_ref().filter(|s| s.repo && s.totals.files > 0) {
            let t = &status.totals;
            actions.push(
                div()
                    .flex_none()
                    .mr(px(4.0))
                    .whitespace_nowrap()
                    .text_size(px(11.5))
                    .text_color(rgb(style.palette.muted))
                    .child(format!("{} \u{b7} {}", model::totals(t.added, t.removed), model::files_count(t.files as usize)))
                    .into_any_element(),
            );
        }
        let list = self.state.list;
        actions.push(
            keep_press(button("changes-list", "sidebar-right", if list { "Hide Files" } else { "Show Files" }, style.palette.icon, true, style))
                .when(list, |el| el.bg(style.palette.selected()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_list(cx)))
                .into_any_element(),
        );
        parts.actions = actions;
        parts
    }
}

impl ChangesPane {
    /// `[Working ▾]`: opens the scope picker.
    fn scope_button(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        let open = self.picker.is_some();
        keep_press(
            div()
                .id("changes-scope")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .h(px(BUTTON))
                .px(px(8.0))
                .continuous(style.control)
                .bg(if open { palette.selected() } else { palette.hover() })
                .cursor_pointer()
                .hover(|s| s.bg(palette.selected()))
                .text_color(rgb(style.ink()))
                .child(scope_label(&self.state.scope))
                .child(crate::assets::icon("chevron-down", 10.0, palette.icon)),
        )
        .tooltip(crate::tooltip::Tooltip::text("What to compare"))
        .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx)))
        .into_any_element()
    }

    /// Unified | Split.
    fn split_switch(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        let split = self.state.split;
        let option = |id: &'static str, label: &'static str, on: bool, value: bool| {
            keep_press(
                div()
                    .id(id)
                    .px(px(7.0))
                    .h(px(BUTTON - 4.0))
                    .flex()
                    .items_center()
                    .continuous(style.control - 2.0)
                    .text_size(px(11.5))
                    .text_color(rgb(if on { palette.ink } else { palette.muted }))
                    .when(on, |el| el.bg(palette.selected()))
                    .when(!on, |el| el.cursor_pointer().hover(|s| s.bg(palette.hover())))
                    .child(label),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.set_split(value, cx)))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(1.0))
            .child(option("changes-unified", "Unified", !split, false))
            .child(option("changes-split", "Split", split, true))
            .into_any_element()
    }

    /// The summary's place before the file tabs.
    fn summary_tab(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        let shown = self.state.active.is_none();
        keep_press(
            div()
                .id("changes-summary")
                .flex()
                .flex_none()
                .items_center()
                .h(px(BUTTON))
                .px(px(8.0))
                .continuous(style.control)
                .when(shown, |el| el.bg(palette.selected()))
                .when(!shown, |el| el.cursor_pointer().hover(|s| s.bg(palette.hover())))
                .text_color(rgb(if shown && style.active { palette.ink } else { palette.secondary }))
                .child("All files"),
        )
        .on_click(cx.listener(|this, _, _, cx| this.show_summary(cx)))
        .into_any_element()
    }

    fn tabs(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let active = self.state.active.as_deref();
        let tabs = self
            .state
            .open
            .iter()
            .zip(tab_labels(&self.state.open))
            .map(|(path, (name, dir))| crate::header_tabs::Tab {
                name,
                dir,
                tooltip: path.clone(),
                shown: active == Some(path.as_str()),
                dirty: false,
                dot: false,
            })
            .collect();
        let paths = std::rc::Rc::new(self.state.open.clone());
        let (pane, closing) = (cx.entity().downgrade(), cx.entity().downgrade());
        let (open, close) = (paths.clone(), paths);
        crate::header_tabs::strip(
            crate::header_tabs::Ids { strip: "changes-tabs", tab: "changes-tab", close: "changes-tab-close" },
            tabs,
            style,
            move |i, _, cx| {
                let _ = pane.update(cx, |this, cx| this.open(&open[i], cx));
            },
            move |i, _, cx| {
                let _ = closing.update(cx, |this, cx| this.close(&close[i], cx));
            },
        )
    }

    fn measure(window: &Window) -> Metrics {
        let (_, family, size) = theme::summary();
        let size = size.clamp(11.0, 16.0);
        let font = gpui::font(SharedString::from(family));
        let id = window.text_system().resolve_font(&font);
        let cell = window.text_system().advance(id, px(size), 'm').map_or(size * 0.6, |a| f32::from(a.width));
        Metrics { size, line: (size * 1.5).round(), cell }
    }

    /// The width of a line number column for the shown hunks.
    fn number_width(&self, m: Metrics) -> f32 {
        let widest = self
            .shown
            .files
            .iter()
            .flat_map(|l| &l.diff.hunks)
            .map(|h| (h.old_start + h.old_lines).max(h.new_start + h.new_lines))
            .max()
            .unwrap_or(1);
        #[allow(clippy::cast_precision_loss)]
        let digits = widest.to_string().len().max(2) as f32;
        digits * m.cell + 12.0
    }

    // ------------------------------------------------------------ the diff

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.state.root.is_empty() {
            return centered(hint("Loading…"));
        }
        if let Some(error) = &self.status_error {
            return centered(div().flex().flex_col().items_center().gap(px(4.0)).child("Couldn't read the changes").child(hint(error.clone())));
        }
        let Some(status) = &self.status else { return centered(hint("Loading…")) };
        let tree = matches!(self.state.scope, silo_proto::model::ChangesScope::Tree { .. });
        if !status.repo && !tree {
            return centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child("Not a git repository"))
                    .child(hint(self.state.root.clone()))
                    .child(hint("An agent's turns still show here once it has some.").text_size(px(12.0))),
            );
        }
        if let Some(active) = &self.state.active
            && let Some(error) = self.diff_errors.get(active)
        {
            return centered(div().flex().flex_col().items_center().gap(px(4.0)).child(format!("Couldn't read {active}")).child(hint(error.clone())));
        }
        if status.files.is_empty() && self.state.active.is_none() {
            return centered(hint(format!("No changes in {}", scope_label(&self.state.scope))));
        }
        if self.shown.rows.is_empty() {
            return centered(hint("Loading…"));
        }
        let m = Self::measure(window);
        let (_, family, _) = theme::summary();
        let list = gpui::list(self.list.clone(), cx.processor(move |this, ix: usize, window, cx| this.render_row(ix, m, window, cx)))
            .size_full()
            .font_family(SharedString::from(family))
            .text_size(px(m.size))
            .line_height(px(m.line));
        let width = self.body_width.clone();
        div()
            .size_full()
            .pb(px(8.0))
            .child(list)
            .child(gpui::canvas(move |bounds, _, _| width.set(f32::from(bounds.size.width)), |_, _, _, _| {}).absolute().size_full())
            .on_scroll_wheel(cx.listener(move |this, e: &gpui::ScrollWheelEvent, _, cx| {
                let d = e.delta.pixel_delta(px(m.line));
                let (dx, dy) = (f32::from(d.x), f32::from(d.y));
                // ⇧ turns a mouse wheel sideways.
                let dx = if e.modifiers.shift && dx == 0.0 { dy } else { dx };
                if dx.abs() <= dy.abs() && !e.modifiers.shift {
                    return;
                }
                let next = sideways(this.scroll_x - dx, this.widest_line(m), this.code_width(m));
                if next != this.scroll_x {
                    this.scroll_x = next;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// The longest shown line's width (px).
    fn widest_line(&self, m: Metrics) -> f32 {
        let chars = self.shown.files.iter().flat_map(|l| &l.diff.hunks).flat_map(|h| &h.lines).map(|l| l.text.chars().count()).max().unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let chars = chars as f32;
        chars * m.cell
    }

    /// The width code shows in (one side's, when split).
    fn code_width(&self, m: Metrics) -> f32 {
        let numbers = self.number_width(m);
        let gutter = m.cell + 6.0 + PAD;
        let width = self.body_width.get();
        if self.state.split { width / 2.0 - numbers - gutter } else { width - numbers * 2.0 - gutter }
    }

    fn render_row(&mut self, ix: usize, m: Metrics, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.shown.rows.get(ix).cloned() else { return div().into_any_element() };
        let tints = Tints::now();
        let numbers = self.number_width(m);
        let hunk_of = match &row {
            model::Row::Hunk { file, hunk }
            | model::Row::Line { file, hunk, .. }
            | model::Row::Pair { file, hunk, .. }
            | model::Row::Fold { file, hunk, .. } => Some((*file, *hunk)),
            _ => None,
        };
        let el = match row {
            model::Row::File { file } => self.render_file_header(file, cx),
            model::Row::Gap { lines, .. } => div()
                .h(px(m.line))
                .pl(px(numbers * 2.0 + PAD))
                .flex()
                .items_center()
                .text_size(px(m.size - 1.5))
                .text_color(rgb(theme::muted()))
                .child(format!("\u{22ef} {lines} unchanged lines"))
                .into_any_element(),
            model::Row::Hunk { file, hunk } => self.render_hunk_header(file, hunk, m, tints, cx),
            model::Row::Fold { file, hunk, at, lines } => div()
                .id(("changes-fold", ix))
                .h(px(m.line))
                .pl(px(numbers * 2.0 + PAD))
                .flex()
                .items_center()
                .gap(px(6.0))
                .bg(rgb(tints.empty))
                .text_size(px(m.size - 1.5))
                .text_color(rgb(theme::muted()))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::foreground())))
                .child(crate::assets::icon("chevron-down", 10.0, theme::muted()))
                .child(format!("{lines} lines"))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_fold(file, hunk, at, cx)))
                .into_any_element(),
            model::Row::Line { file, hunk, at } => self.render_line(ix, file, hunk, at, m, numbers, tints, cx),
            model::Row::Pair { file, hunk, old, new } => {
                let left = self.render_half(ix, file, hunk, old, Side::Old, m, numbers, tints, cx);
                let right = self.render_half(ix, file, hunk, new, Side::New, m, numbers, tints, cx);
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .child(div().flex_1().min_w_0().overflow_hidden().border_r_1().border_color(rgb(theme::border())).child(left))
                    .child(div().flex_1().min_w_0().overflow_hidden().child(right))
                    .into_any_element()
            }
            model::Row::Comment { index, .. } => self.render_comment(index, numbers, cx),
            model::Row::Note { text, .. } => div()
                .py(px(6.0))
                .pl(px(numbers + PAD))
                .text_size(px(m.size - 1.0))
                .text_color(rgb(theme::muted()))
                .italic()
                .child(text)
                .into_any_element(),
        };
        // Hovering any of a hunk's rows shows its bar's actions.
        div()
            .id(("changes-row", ix))
            .w_full()
            .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                let next = if *over { hunk_of } else { this.hovered.filter(|h| Some(*h) != hunk_of) };
                if this.hovered != next {
                    this.hovered = next;
                    cx.notify();
                }
            }))
            .child(el)
            .into_any_element()
    }

    fn render_file_header(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(loaded) = self.shown.files.get(file) else { return div().into_any_element() };
        let path = loaded.diff.path.clone();
        let changed = self.status.as_ref().and_then(|s| s.files.iter().find(|f| f.path == path));
        let counts = changed.map(model::file_counts).unwrap_or_default();
        let name = crate::files::model::name(&path).to_owned();
        let dir = path.strip_suffix(name.as_str()).unwrap_or("").trim_end_matches('/').to_owned();
        let moved = loaded.diff.old_path.clone().map(|o| format!("from {o}"));
        let (open, files) = (path.clone(), path.clone());
        let line = loaded.diff.hunks.first().map_or(1, |h| h.new_start.max(1));
        div()
            .id(SharedString::from(format!("changes-file-{path}")))
            .mt(px(if file == 0 { 6.0 } else { 14.0 }))
            .mx(px(8.0))
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .continuous(8.0)
            .bg(theme::row_fill())
            .font_family(theme::ui_font())
            .text_size(px(12.5))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child(crate::assets::icon("file-text", 13.0, theme::muted()))
            .child(div().flex_none().font_weight(gpui::FontWeight::MEDIUM).child(name))
            .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(rgb(theme::muted())).child(dir))
            .children(moved.map(|m| div().flex_none().text_color(rgb(theme::muted())).child(m)))
            .child(div().flex_1())
            .child(div().flex_none().text_size(px(11.5)).text_color(rgb(theme::muted())).child(counts))
            .tooltip(crate::tooltip::Tooltip::text("Open its tab  (⌘-click: in Files)"))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                if e.modifiers().platform {
                    this.open_in_files(&files, line, cx);
                } else {
                    this.open(&open, cx);
                }
            }))
            .into_any_element()
    }

    fn render_hunk_header(&self, file: usize, hunk: usize, m: Metrics, tints: Tints, cx: &mut Context<Self>) -> AnyElement {
        let Some(loaded) = self.shown.files.get(file) else { return div().into_any_element() };
        let Some(h) = loaded.diff.hunks.get(hunk) else { return div().into_any_element() };
        let (path, id) = (loaded.diff.path.clone(), h.id.clone());
        let current = self.current.as_ref().is_some_and(|(p, i)| *p == path && *i == id);
        let shown = current || self.hovered == Some((file, hunk));
        let mode = self.mode();
        let action = |label: &'static str, key: &'static str, act: HunkAction| {
            let (path, id) = (path.clone(), id.clone());
            div()
                .id(SharedString::from(format!("changes-{label}-{path}-{id}")))
                .px(px(7.0))
                .h(px(m.line - 4.0))
                .flex()
                .items_center()
                .continuous(5.0)
                .bg(theme::control_fill())
                .cursor_pointer()
                .hover(|s| s.bg(theme::row_fill()))
                .child(label)
                .tooltip(crate::tooltip::Tooltip::text(key))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.current = Some((path.clone(), id.clone()));
                    this.hunk_action(act, cx);
                }))
        };
        let mut actions = div().flex().flex_none().items_center().gap(px(4.0)).font_family(theme::ui_font()).text_size(px(11.5)).opacity(if shown {
            1.0
        } else {
            0.0
        });
        match mode {
            model::ListMode::Working | model::ListMode::Unstaged => {
                actions = actions.child(action("Stage", "s", HunkAction::Stage)).child(action("Revert", "r", HunkAction::Revert));
            }
            model::ListMode::Staged => actions = actions.child(action("Unstage", "u", HunkAction::Unstage)),
            model::ListMode::ReadOnly => {}
        }
        let (cpath, cid) = (path.clone(), id.clone());
        let comment = div()
            .id(SharedString::from(format!("changes-comment-{path}-{id}")))
            .px(px(7.0))
            .h(px(m.line - 4.0))
            .flex()
            .items_center()
            .continuous(5.0)
            .bg(theme::control_fill())
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
            .child("Comment")
            .tooltip(crate::tooltip::Tooltip::text("c"))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.current = Some((cpath.clone(), cid.clone()));
                this.selection = None;
                this.start_comment(window, cx);
            }));
        actions = actions.child(comment);
        let (gpath, gid) = (path.clone(), id.clone());
        div()
            .id(SharedString::from(format!("changes-hunk-{path}-{id}")))
            .w_full()
            .h(px(m.line + 4.0))
            .mt(px(4.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(PAD))
            .pr(px(8.0))
            .bg(rgb(tints.hunk))
            .when(current, |el| el.border_l_2().border_color(rgb(theme::accent())))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(m.size - 1.0))
                    .text_color(rgb(theme::muted()))
                    .child(hunk_title(h)),
            )
            .child(actions)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.current = Some((gpath.clone(), gid.clone()));
                cx.notify();
            }))
            .into_any_element()
    }

    /// One line's text: syntax colours with the changed words' backgrounds over them.
    fn styled(loaded: &Loaded, hunk: usize, at: usize, word: Hsla, bg: u32) -> StyledText {
        let line = &loaded.diff.hunks[hunk].lines[at];
        let src = line.text.as_str();
        let mut shown = expand_tabs(src).into_owned();
        if shown.is_empty() {
            shown.push(' ');
        }
        let map = |r: &Range<usize>| to_shown(src, r.start.min(src.len()))..to_shown(src, r.end.min(src.len()));
        let spans: Vec<_> = loaded
            .syntax
            .as_ref()
            .and_then(|s| s.get(hunk)?.get(at).cloned())
            .map(|spans| crate::syntax::styles(&spans, bg))
            .unwrap_or_default()
            .iter()
            .map(|(r, s)| (map(r), *s))
            .filter(|(r, _)| r.start < r.end)
            .collect();
        let backs: Vec<(Range<usize>, Hsla)> =
            loaded.words.get(hunk).and_then(|h| h.get(at)).map(|w| w.iter().map(|r| (map(r), word)).collect()).unwrap_or_default();
        StyledText::new(shown).with_highlights(crate::files::text::combine(&spans, &backs))
    }

    /// A line number cell: a click picks the line (⇧ extends), ⌘-click opens it in Files.
    fn number(&self, id: (&'static str, usize), path: &str, side: Side, line: Option<u32>, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let text = line.map(|n| n.to_string()).unwrap_or_default();
        let cell = div().id(id).flex_none().w(px(width)).pr(px(8.0)).flex().justify_end().text_color(rgb(theme::muted())).child(text);
        let Some(n) = line else { return cell.into_any_element() };
        let path = path.to_owned();
        cell.cursor_pointer()
            .hover(|s| s.text_color(rgb(theme::foreground())))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                if e.modifiers().platform {
                    this.open_in_files(&path, n, cx);
                } else {
                    this.pick_line(&path, side, n, e.modifiers().shift, cx);
                }
            }))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &self,
        ix: usize,
        file: usize,
        hunk: usize,
        at: usize,
        m: Metrics,
        numbers: f32,
        tints: Tints,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(loaded) = self.shown.files.get(file).cloned() else { return div().into_any_element() };
        let line = &loaded.diff.hunks[hunk].lines[at];
        if matches!(line.kind, LineKind::NoNewline | LineKind::Unknown) {
            return div()
                .h(px(m.line))
                .pl(px(numbers * 2.0 + PAD + m.cell + 6.0))
                .text_size(px(m.size - 1.5))
                .text_color(rgb(theme::muted()))
                .italic()
                .child(line.text.clone())
                .into_any_element();
        }
        let path = loaded.diff.path.clone();
        let (side, n) = match line.kind {
            LineKind::Removed => (Side::Old, line.old),
            _ => (Side::New, line.new),
        };
        let picked = n.is_some_and(|n| self.selection.as_ref().is_some_and(|s| s.contains(&path, side, n)));
        let bg = tints.line(line.kind).unwrap_or_else(theme::background);
        let word = if line.kind == LineKind::Removed { tints.removed_word } else { tints.added_word };
        let sign = match line.kind {
            LineKind::Added => "+",
            LineKind::Removed => "\u{2212}",
            _ => " ",
        };
        let mark = self.mark_on(&path, side, n);
        div()
            .h(px(m.line))
            .w_full()
            .flex()
            .flex_row()
            .whitespace_nowrap()
            .when_some(tints.line(line.kind), |el, c| el.bg(rgb(c)))
            .when(picked, |el| el.bg(alpha(theme::selection(), 0.55)))
            .child(self.number(("changes-old", ix), &path, Side::Old, line.old.filter(|_| line.kind != LineKind::Added), numbers, cx))
            .child(self.number(("changes-new", ix), &path, Side::New, line.new.filter(|_| line.kind != LineKind::Removed), numbers, cx))
            .child(div().flex_none().w(px(m.cell + 6.0)).text_color(rgb(theme::muted())).child(sign))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .pr(px(PAD))
                    .child(div().ml(px(-self.scroll_x)).child(Self::styled(&loaded, hunk, at, word, bg))),
            )
            .children(mark)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_half(
        &self,
        ix: usize,
        file: usize,
        hunk: usize,
        at: Option<usize>,
        side: Side,
        m: Metrics,
        numbers: f32,
        tints: Tints,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(loaded) = self.shown.files.get(file).cloned() else { return div().into_any_element() };
        let Some(at) = at else { return div().h(px(m.line)).w_full().bg(rgb(tints.empty)).into_any_element() };
        let line = &loaded.diff.hunks[hunk].lines[at];
        let path = loaded.diff.path.clone();
        let n = if side == Side::Old { line.old } else { line.new };
        let picked = n.is_some_and(|n| self.selection.as_ref().is_some_and(|s| s.contains(&path, side, n)));
        let tint = tints.line(line.kind);
        let bg = tint.unwrap_or_else(theme::background);
        let word = if line.kind == LineKind::Removed { tints.removed_word } else { tints.added_word };
        let sign = match line.kind {
            LineKind::Added => "+",
            LineKind::Removed => "\u{2212}",
            _ => " ",
        };
        let id = if side == Side::Old { "changes-left" } else { "changes-right" };
        let mark = self.mark_on(&path, side, n);
        div()
            .h(px(m.line))
            .w_full()
            .flex()
            .flex_row()
            .whitespace_nowrap()
            .when_some(tint, |el, c| el.bg(rgb(c)))
            .when(picked, |el| el.bg(alpha(theme::selection(), 0.55)))
            .child(self.number((id, ix), &path, side, n, numbers, cx))
            .child(div().flex_none().w(px(m.cell + 6.0)).text_color(rgb(theme::muted())).child(sign))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .pr(px(PAD))
                    .child(div().ml(px(-self.scroll_x)).child(Self::styled(&loaded, hunk, at, word, bg))),
            )
            .children(mark)
            .into_any_element()
    }

    /// An agent's mark on a new line (`changes_show`): a dot, its note on hover.
    fn mark_on(&self, path: &str, side: Side, line: Option<u32>) -> Option<AnyElement> {
        let line = line?;
        let mark = self.state.marks.iter().find(|m| side == Side::New && m.path == path && m.line == line)?;
        let id = SharedString::from(format!("changes-mark-{path}-{line}"));
        let el =
            div().id(id).flex_none().w(px(16.0)).h_full().flex().items_center().child(div().size(px(7.0)).rounded_full().bg(rgb(theme::warning())));
        Some(match mark.note.clone() {
            Some(note) => el.tooltip(crate::tooltip::Tooltip::text(note)).into_any_element(),
            None => el.into_any_element(),
        })
    }

    fn render_comment(&self, index: usize, numbers: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(c) = self.state.comments.get(index) else { return div().into_any_element() };
        let place = match c.end_line {
            Some(end) if end != c.line => format!("{}:{}\u{2013}{end}", crate::files::model::name(&c.path), c.line),
            _ => format!("{}:{}", crate::files::model::name(&c.path), c.line),
        };
        let id = c.id.clone();
        div()
            .pl(px(numbers + PAD))
            .pr(px(PAD * 2.0))
            .py(px(4.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .continuous(8.0)
                    .bg(theme::row_fill())
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .font_family(theme::ui_font())
                    .text_size(px(12.5))
                    .child(div().pt(px(2.0)).child(crate::assets::icon("message-square", 12.0, theme::accent())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(11.0)).text_color(rgb(theme::muted())).child(place))
                            .child(div().whitespace_normal().child(c.text.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("changes-uncomment-{id}")))
                            .flex_none()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .continuous(4.0)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover_fill()))
                            .child(crate::assets::icon("close", 10.0, theme::muted()))
                            .tooltip(crate::tooltip::Tooltip::text("Delete the comment"))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_comment(&id, cx))),
                    ),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------ the list

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let files = self.status.as_ref().map(|s| s.files.as_slice()).unwrap_or_default();
        let rows = model::group(files, self.mode());
        let active = self.state.active.clone();
        let busy = self.busy.is_some();
        let items = rows.into_iter().enumerate().map(|(i, row)| match row {
            model::ListRow::Section { section, count } => div()
                .h(px(ROW))
                .mt(px(if i == 0 { 2.0 } else { 8.0 }))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::muted()))
                .child(section.title())
                .child(div().child(count.to_string()))
                .into_any_element(),
            model::ListRow::File { file, section, checked } => {
                let f = &files[file];
                let name = crate::files::model::name(&f.path).to_owned();
                let dir = f.path.strip_suffix(name.as_str()).unwrap_or("").trim_end_matches('/').to_owned();
                let shown = active.as_deref() == Some(f.path.as_str());
                let (open, toggle) = (f.path.clone(), f.path.clone());
                let line = 1;
                let checkbox = checked.map(|on| {
                    div()
                        .id(SharedString::from(format!("changes-check-{section:?}-{}", f.path)))
                        .flex_none()
                        .size(px(14.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .continuous(4.0)
                        .border_1()
                        .border_color(rgb(if on { theme::accent() } else { theme::border() }))
                        .when(on, |el| el.bg(rgb(theme::accent())))
                        .when(!busy, |el| el.cursor_pointer())
                        .children(on.then(|| crate::assets::icon("check", 10.0, theme::on_accent())))
                        .tooltip(crate::tooltip::Tooltip::text(if on { "Unstage" } else { "Stage" }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if on {
                                this.unstage(&toggle, None, cx);
                            } else {
                                this.stage(&toggle, None, cx);
                            }
                        }))
                });
                div()
                    .id(SharedString::from(format!("changes-row-{section:?}-{}", f.path)))
                    .h(px(ROW))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(shown, |el| el.bg(theme::row_fill()))
                    .when(!shown, |el| el.hover(|s| s.bg(theme::hover_fill())))
                    .cursor_pointer()
                    .children(checkbox)
                    .child(div().flex_none().text_color(rgb(theme::foreground())).child(name))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(rgb(theme::muted())).child(dir))
                    .child(div().flex_none().text_size(px(11.5)).text_color(rgb(theme::muted())).child(model::file_counts(f)))
                    .tooltip(crate::tooltip::Tooltip::text(SharedString::from(f.path.clone())))
                    .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                        if e.modifiers().platform {
                            this.open_in_files(&open, line, cx);
                        } else {
                            this.open(&open, cx);
                        }
                    }))
                    .into_any_element()
            }
        });
        let head = self.status.as_ref().map(|s| {
            let branch = s.branch.clone().or_else(|| s.head.clone().map(|h| format!("detached at {h}"))).unwrap_or_default();
            let sync = match (s.ahead, s.behind) {
                (0, 0) => String::new(),
                (a, 0) => format!(" \u{2191}{a}"),
                (0, b) => format!(" \u{2193}{b}"),
                (a, b) => format!(" \u{2191}{a} \u{2193}{b}"),
            };
            div()
                .h(px(ROW + 4.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .text_size(px(12.0))
                .text_color(rgb(theme::muted()))
                .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(format!("{branch}{sync}")))
                .when(s.truncated, |el| el.child(div().flex_none().child("· truncated")))
        });
        let empty = files.is_empty().then(|| div().p(px(12.0)).child(hint("No changed files")));
        div()
            .id("changes-list-panel")
            .flex_none()
            .w(px(super::LIST_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .children(head)
            .child(div().id("changes-list-rows").flex_1().min_h_0().overflow_y_scroll().py(px(4.0)).children(items).children(empty))
            .into_any_element()
    }

    // ------------------------------------------------------------ bars

    fn render_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (text, error) = self.notice.clone()?;
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .bg(rgb(theme::mix(theme::background(), if error { theme::danger() } else { theme::success() }, 0.12)))
                .text_size(px(12.5))
                .child(div().flex_1().min_w_0().whitespace_normal().child(text))
                .child(
                    div()
                        .id("changes-notice-close")
                        .flex_none()
                        .size(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .continuous(4.0)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_fill()))
                        .child(crate::assets::icon("close", 10.0, theme::muted()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_composing(&self) -> Option<AnyElement> {
        let c = self.composing.as_ref()?;
        let side = if c.side == Side::Old { "old" } else { "new" };
        let place = match c.end {
            Some(end) => format!("{}:{}\u{2013}{end} ({side})", crate::files::model::name(&c.path), c.line),
            None => format!("{}:{} ({side})", crate::files::model::name(&c.path), c.line),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(12.0))
                .py(px(8.0))
                .border_t_1()
                .border_color(rgb(theme::border()))
                .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(format!("Comment on {place}  ·  esc to cancel")))
                .child(div().h(px(28.0)).px(px(8.0)).flex().items_center().continuous(6.0).bg(theme::well_fill()).child(c.field.clone()))
                .into_any_element(),
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let commits = self.commits();
        let comments = self.state.comments.len();
        if !commits && comments == 0 {
            return None;
        }
        let busy = self.busy;
        let pill = |id: &'static str, label: SharedString, primary: bool| {
            div()
                .id(id)
                .flex_none()
                .h(px(26.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .continuous(7.0)
                .text_size(px(12.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
                .when(!primary, |el| el.bg(theme::control_fill()).hover(|s| s.bg(theme::row_fill())))
                .when(busy.is_none(), |el| el.cursor_pointer())
                .when(busy.is_some(), |el| el.opacity(0.6))
                .child(label)
        };
        let mut row = div().flex().flex_row().items_end().gap(px(8.0));
        if commits {
            let colors = EditorColors {
                text: rgb(theme::foreground()).into(),
                placeholder: rgb(theme::muted()).into(),
                caret: rgb(theme::foreground()).into(),
                selection: alpha(theme::selection(), 0.6),
            };
            let amend = self.amend;
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(px(10.0))
                        .py(px(5.0))
                        .continuous(7.0)
                        .bg(theme::well_fill())
                        .text_size(px(12.5))
                        .child(Editor::element(&self.commit, 4, colors, cx)),
                )
                .child(
                    pill("changes-ask", if self.asking { "Asking…".into() } else { "Ask agent".into() }, false)
                        .tooltip(crate::tooltip::Tooltip::text("Ask the agent for a commit message"))
                        .on_click(cx.listener(|this, _, _, cx| this.ask_commit(cx))),
                )
                .child(
                    pill("changes-amend", "Amend".into(), false)
                        .when(amend, |el| el.bg(theme::row_fill()).border_1().border_color(rgb(theme::accent())))
                        .tooltip(crate::tooltip::Tooltip::text("Fold the staged changes into the last commit"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.amend = !this.amend;
                            cx.notify();
                        })),
                )
                .child(
                    pill("changes-commit", busy.map_or_else(|| if amend { "Amend ⌘↩".into() } else { "Commit ⌘↩".into() }, SharedString::from), true)
                        .on_click(cx.listener(|this, _, _, cx| this.commit_now(cx))),
                );
        } else {
            row = row.child(div().flex_1());
        }
        if comments > 0 {
            let label = if comments == 1 { "1 comment".to_owned() } else { format!("{comments} comments") };
            row = row
                .child(div().flex_none().h(px(26.0)).flex().items_center().text_size(px(12.5)).text_color(rgb(theme::muted())).child(label))
                .child(
                    pill("changes-send", "Send review".into(), !commits)
                        .tooltip(crate::tooltip::Tooltip::text("Queue the comments on the agent  ⌥⌘↩"))
                        .on_click(cx.listener(|this, _, _, cx| this.send_review(cx))),
                );
        }
        Some(div().flex_none().px(px(10.0)).py(px(8.0)).border_t_1().border_color(rgb(theme::border())).child(row).into_any_element())
    }

    fn render_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.picker.as_ref()?;
        let current = &self.state.scope;
        let rows = picker.rows.iter().enumerate().map(|(i, pick)| {
            let on = super::scope_of(pick).as_ref() == Some(current);
            let selected = i == picker.selected;
            div()
                .id(("changes-pick", i))
                .h(px(26.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .continuous(6.0)
                .when(selected, |el| el.bg(rgb(theme::row_active())))
                .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
                .cursor_pointer()
                .child(div().w(px(12.0)).flex_none().children(on.then(|| crate::assets::icon("check", 11.0, theme::accent()))))
                .child(div().flex_1().child(pick.label()))
                .children(pick.detail().map(|d| div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(d)))
                .on_click(cx.listener(move |this, _, window, cx| this.pick(i, window, cx)))
        });
        let branch = picker.branch.as_ref().map(|(field, _)| {
            div().mx(px(4.0)).mt(px(4.0)).h(px(28.0)).px(px(8.0)).flex().items_center().continuous(6.0).bg(theme::well_fill()).child(field.clone())
        });
        Some(
            div()
                .id("changes-picker")
                .absolute()
                .top(px(6.0))
                .left(px(8.0))
                .w(px(260.0))
                .flex()
                .flex_col()
                .p(px(4.0))
                .continuous(10.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .text_size(px(13.0))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(rows)
                .children(branch)
                .into_any_element(),
        )
    }

    fn key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let m = k.modifiers;
        let key = k.key.as_str();
        let own = self.focus.is_focused(window);
        let picking = self.picker.as_ref().is_some_and(|p| p.branch.is_none());
        let handled = match key {
            "enter" if m.platform && m.alt => {
                self.send_review(cx);
                true
            }
            "tab" if m.control && !m.platform && !m.alt => {
                self.step_tab(if m.shift { -1 } else { 1 }, cx);
                true
            }
            "escape" if self.picker.is_some() => {
                self.picker = None;
                window.focus(&self.focus, cx);
                true
            }
            "up" | "down" if picking => {
                if let Some(p) = self.picker.as_mut() {
                    p.selected = crate::launcher::step(p.selected, p.rows.len(), if key == "up" { -1 } else { 1 });
                }
                true
            }
            "enter" if picking => {
                let at = self.picker.as_ref().map_or(0, |p| p.selected);
                self.pick(at, window, cx);
                true
            }
            _ if !own || m.platform || m.control || m.alt || m.function => false,
            "j" => {
                self.step_hunk(1, cx);
                true
            }
            "k" => {
                self.step_hunk(-1, cx);
                true
            }
            "n" => {
                self.step_file(1, cx);
                true
            }
            "p" => {
                self.step_file(-1, cx);
                true
            }
            "s" => {
                self.hunk_action(HunkAction::Stage, cx);
                true
            }
            "u" => {
                self.hunk_action(HunkAction::Unstage, cx);
                true
            }
            "r" => {
                self.hunk_action(HunkAction::Revert, cx);
                true
            }
            "c" => {
                self.start_comment(window, cx);
                true
            }
            "escape" if self.selection.is_some() || self.current.is_some() => {
                self.selection = None;
                self.current = None;
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }
}

impl Render for ChangesPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.render_notice(cx);
        let body = self.render_body(window, cx);
        let list = self.state.list.then(|| self.render_list(cx));
        let composing = self.render_composing();
        let footer = self.render_footer(cx);
        let picker = self.render_picker(cx);
        let bounds = self.body_bounds.clone();
        let probe = gpui::canvas(move |b, _, _| b, move |b, _, _, _| bounds.set(Some(b))).absolute().top_0().left_0().size_full();
        div()
            .id("changes-pane")
            .key_context("ChangesPane")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(13.0))
            .text_color(rgb(theme::foreground()))
            .on_action(cx.listener(|this, _: &act::NewPane, window, cx| {
                // ⌘↩ commits here (⌘↩ is New Pane elsewhere).
                if this.commits() && this.focus.contains_focused(window, cx) {
                    this.commit_now(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &act::ClosePane, window, cx| {
                if this.picker.is_some() || this.composing.is_some() {
                    this.picker = None;
                    this.composing = None;
                    window.focus(&this.focus, cx);
                    return cx.notify();
                }
                match this.state.active.clone() {
                    Some(path) => this.close(&path, cx),
                    // The summary: ⌘W closes the block.
                    None => cx.propagate(),
                }
            }))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.picker.take().is_some() {
                        cx.notify();
                    }
                    if !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .children(notice)
            .child(div().flex_1().min_h_0().flex().flex_row().child(div().flex_1().min_w_0().h_full().child(body)).children(list))
            .children(composing)
            .children(footer)
            .children(picker)
            .child(probe)
    }
}
