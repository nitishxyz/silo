//! Search in files (⇧⌘F; host feature `search`): the server's `search.grep`
//! over the root (the palette's Grep chip asks the same), the matches by
//! file, ↩ or a click opens the file at the line.

use std::{ops::Range, time::Duration};

use gpui::{AppContext as _, Context, Entity, Subscription, Task, Window};
use silo_proto::{compat::feature, control::FilesSetParams};

use super::FilesPane;
use crate::{
    finder::{GrepHit, Hits, Request},
    finder_search::{self, Reply},
    text_field::{FieldEvent, TextField},
};

/// Matches asked for.
const LIMIT: usize = 300;
/// Typing settles this long before a search.
const SETTLE: Duration = Duration::from_millis(120);

/// The search panel.
pub(crate) struct Search {
    pub field: Entity<TextField>,
    pub hits: Option<Hits<GrepHit>>,
    /// The selected match (an index into the hits).
    pub selected: usize,
    pub error: Option<String>,
    pub scroll: gpui::ScrollHandle,
    task: Option<Task<()>>,
    _sub: Subscription,
}

/// A row of the results: a file's header, or one of its matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    File { path: String, count: usize },
    Hit(usize),
}

/// The results by file, in the server's order (each file's matches together).
#[must_use]
pub fn rows(hits: &[GrepHit]) -> Vec<Row> {
    let mut out = Vec::new();
    let mut header = None;
    for (i, hit) in hits.iter().enumerate() {
        if hits.get(i.wrapping_sub(1)).is_none_or(|prev| prev.path != hit.path) {
            header = Some(out.len());
            out.push(Row::File { path: hit.path.clone(), count: 0 });
        }
        if let Some(Row::File { count, .. }) = header.and_then(|h| out.get_mut(h)) {
            *count += 1;
        }
        out.push(Row::Hit(i));
    }
    out
}

/// A match's line as shown: leading blanks dropped (its ranges moved with
/// it), tabs as 4 spaces; ranges past the text are dropped.
#[must_use]
pub fn shown_line(text: &str, ranges: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let start = text.len() - text.trim_start().len();
    let mut out = String::new();
    let mut map = Vec::with_capacity(text.len() + 1);
    for (i, c) in text.char_indices().skip_while(|(i, _)| *i < start) {
        map.resize(i - start, out.len());
        map.push(out.len());
        if c == '\t' { out.push_str("    ") } else { out.push(c) }
    }
    map.resize(text.len() - start + 1, out.len());
    let ranges = ranges
        .iter()
        .filter(|r| r.end <= text.len() && r.start < r.end && r.end > start)
        .map(|r| map[r.start.saturating_sub(start)]..map[r.end - start])
        .filter(|r| r.start < r.end)
        .collect();
    (out, ranges)
}

impl FilesPane {
    /// The host searches file contents.
    pub(crate) fn searches(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.peer_supports(feature::SEARCH))
    }

    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.searches() {
            return;
        }
        self.finder = None;
        if let Some(search) = &self.search {
            let focus = search.field.read(cx).focus.clone();
            search.field.update(cx, |f, cx| f.select_all(cx));
            return focus.focus(window, cx);
        }
        let field = cx.new(|cx| TextField::new("Search in files", cx).on_glass());
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.search_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        self.search = Some(Search { field, hits: None, selected: 0, error: None, scroll: gpui::ScrollHandle::new(), task: None, _sub: sub });
        focus.focus(window, cx);
        cx.notify();
    }

    fn search_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else { return };
        let len = search.hits.as_ref().map_or(0, |h| h.items.len());
        match event {
            FieldEvent::Changed(text) => {
                let text = text.clone();
                self.search_query(text, cx);
            }
            FieldEvent::Up | FieldEvent::Down => {
                search.selected = crate::launcher::step(search.selected, len, if matches!(event, FieldEvent::Up) { -1 } else { 1 });
                let items = search.hits.as_ref().map_or(&[][..], |h| &h.items[..]);
                if let Some(row) = rows(items).iter().position(|r| *r == Row::Hit(search.selected)) {
                    search.scroll.scroll_to_item(row);
                }
            }
            FieldEvent::Cancel => self.close_bars(window, cx),
            FieldEvent::Submit => {
                let index = search.selected;
                self.search_pick(index, window, cx);
            }
        }
        cx.notify();
    }

    fn search_query(&mut self, query: String, cx: &mut Context<Self>) {
        let (control, root) = (self.control.clone(), self.state.root.clone());
        let Some(search) = self.search.as_mut() else { return };
        if query.trim().is_empty() {
            search.task = None;
            search.hits = None;
            return cx.notify();
        }
        let rt = self.rt.clone();
        search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let request = Request::Grep { query, limit: LIMIT };
            let reply = rt.spawn(finder_search::run(control, root, request)).await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                let Some(search) = this.search.as_mut() else { return };
                match reply {
                    Ok(Reply::Grep(hits)) => {
                        search.hits = Some(hits);
                        search.error = None;
                    }
                    Ok(Reply::Files(_)) => {}
                    Err(error) => search.error = Some(super::describe(&error)),
                }
                search.selected = 0;
                cx.notify();
            });
        }));
    }

    /// Open match `index`'s file with its line in view.
    pub(crate) fn search_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.search.as_ref().and_then(|s| s.hits.as_ref()?.items.get(index).cloned()) else { return };
        self.search = None;
        if self.state.view == silo_proto::model::FilesView::Browse {
            self.set_view(silo_proto::model::FilesView::Edit, window, cx);
        }
        self.open(&hit.path, true, cx);
        let top = finder_search::top_for(hit.line);
        self.state.scroll.insert(hit.path.clone(), top);
        self.applied.remove(&hit.path);
        self.set(FilesSetParams { scroll: Some([(hit.path, top)].into()), ..FilesSetParams::default() }, cx);
        window.focus(&self.focus, cx);
    }
}

impl FilesPane {
    /// The search panel over the viewer.
    pub(super) fn render_search(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use gpui::{HighlightStyle, MouseButton, StyledText, div, prelude::*, px, rgb};

        use crate::{
            surface::Glassy as _,
            theme::{self, Continuous as _},
        };
        let _glass = crate::surface::enter();
        let search = self.search.as_ref()?;
        let muted = rgb(theme::muted());
        let accent = HighlightStyle { color: Some(rgb(theme::accent()).into()), font_weight: Some(gpui::FontWeight::SEMIBOLD), ..Default::default() };
        let items = search.hits.as_ref().map_or(&[][..], |h| &h.items[..]);
        let list = rows(items).into_iter().enumerate().map(|(ix, row)| match row {
            Row::File { path, count } => {
                let name = super::model::name(&path).to_owned();
                let dir = super::model::parent(&path).to_owned();
                div()
                    .id(("files-search-file", ix))
                    .flex()
                    .items_baseline()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .pt(px(if ix == 0 { 2.0 } else { 8.0 }))
                    .pb(px(2.0))
                    .child(div().flex_none().font_weight(gpui::FontWeight::SEMIBOLD).child(name))
                    .child(div().min_w_0().overflow_hidden().text_ellipsis().text_size(px(11.5)).text_color(muted).child(dir))
                    .child(div().flex_none().ml_auto().text_size(px(11.0)).text_color(muted).child(count.to_string()))
            }
            Row::Hit(i) => {
                let hit = &items[i];
                let (text, ranges) = shown_line(&hit.text, &hit.ranges);
                let selected = i == search.selected;
                div()
                    .id(("files-search-hit", ix))
                    .flex()
                    .items_baseline()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .py(px(3.0))
                    .continuous(6.0)
                    .when(selected, |el| el.bg(theme::row_fill()))
                    .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
                    .cursor_pointer()
                    .child(div().flex_none().w(px(36.0)).flex().justify_end().text_size(px(11.0)).text_color(muted).child(hit.line.to_string()))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.0))
                            .child(StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, accent)))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.search_pick(i, window, cx)))
            }
        });
        let footer = match (&search.error, &search.hits) {
            (Some(error), _) => Some(error.clone()),
            (None, Some(hits)) if hits.items.is_empty() => {
                Some(if hits.indexing { "Nothing yet: still indexing the folder".into() } else { "No matches".into() })
            }
            (None, Some(hits)) if hits.total > hits.items.len() => Some(format!("Showing {} of {} matches", hits.items.len(), hits.total)),
            (None, Some(hits)) if hits.indexing => Some("Still indexing the folder: more may come".into()),
            _ => None,
        };
        Some(
            div()
                .absolute()
                .top(px(8.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("files-search")
                        .w(px(620.0))
                        .max_w_full()
                        .mx(px(12.0))
                        .flex()
                        .flex_col()
                        .glass(10.0)
                        .text_size(px(13.0))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close_bars(window, cx)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .px(px(12.0))
                                .h(px(36.0))
                                .when(search.hits.is_some() || search.error.is_some(), |el| el.border_b_1())
                                .border_color(rgb(theme::border()))
                                .child(crate::assets::icon("search", 13.0, theme::muted()))
                                .child(search.field.clone()),
                        )
                        .when(search.hits.is_some() || search.error.is_some(), |el| {
                            el.child(
                                div()
                                    .id("files-search-list")
                                    .max_h(px(420.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&search.scroll)
                                    .p(px(4.0))
                                    .children(list),
                            )
                            .children(footer.map(|f| {
                                div()
                                    .px(px(12.0))
                                    .py(px(5.0))
                                    .text_size(px(11.0))
                                    .text_color(muted)
                                    .border_t_1()
                                    .border_color(rgb(theme::border()))
                                    .child(f)
                            }))
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, line: u32) -> GrepHit {
        GrepHit { path: path.into(), line, text: String::new(), ranges: Vec::new(), git: None }
    }

    #[test]
    fn matches_group_under_their_file() {
        let hits = [hit("a.rs", 1), hit("a.rs", 9), hit("src/b.rs", 4), hit("a.rs", 12)];
        assert_eq!(
            rows(&hits),
            [
                Row::File { path: "a.rs".into(), count: 2 },
                Row::Hit(0),
                Row::Hit(1),
                Row::File { path: "src/b.rs".into(), count: 1 },
                Row::Hit(2),
                Row::File { path: "a.rs".into(), count: 1 },
                Row::Hit(3),
            ]
        );
        assert!(rows(&[]).is_empty());
    }

    #[test]
    fn shown_lines_keep_their_matches() {
        let (text, ranges) = shown_line("    let x = foo(1);", &[8..9, 12..15]);
        assert_eq!((text.as_str(), ranges), ("let x = foo(1);", vec![4..5, 8..11]));
        // A tab inside the line is 4 columns; the match after it moves along.
        let (text, ranges) = shown_line("a\tfoo", &[Range { start: 2, end: 5 }]);
        assert_eq!((text.as_str(), ranges), ("a    foo", vec![Range { start: 5, end: 8 }]));
        // Multi-byte characters, a match in the dropped blanks and one past the end.
        let (text, ranges) = shown_line("  héllo wörld", &[0..1, 2..8, 9..15, 40..41]);
        assert_eq!((text.as_str(), ranges), ("héllo wörld", vec![0..6, 7..13]));
    }
}
