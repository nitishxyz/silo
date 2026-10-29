//! Drawing the Files block's browser (docs/FILES_BLOCK.md §8): a toolbar
//! (back, forward, the path bar, hidden items, New, search), sortable
//! columns, the virtualized list with folders opening in place, the context
//! menu, Quick Look, the upload card, and the browser's keys. Columns have
//! fixed widths, so nothing is measured per frame.

use std::{collections::HashSet, rc::Rc};

use gpui::{
    AnyElement, Context, ExternalPaths, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ObjectFit, SharedString, StyledImage as _, Window,
    anchored, deferred, div, img, prelude::*, px, rgb,
};
use silo_proto::model::FilesView;

use super::{
    Content, FilesPane,
    browse::{self, Row, SortKey},
    browser::{DragPreview, DraggedFiles},
    model,
};
use crate::surface::Glassy as _;
use crate::{
    pane_header::{BUTTON, HeaderStyle, keep_press},
    theme::{self, Continuous as _},
};

/// A list row's height.
pub(super) const ROW: f32 = 24.0;
/// Indent per open folder level.
const INDENT: f32 = 16.0;
const DATE_W: f32 = 158.0;
const SIZE_W: f32 = 74.0;
const KIND_W: f32 = 136.0;
const FOLDER_W: f32 = 170.0;
/// The name column never gets narrower than this to make room for others.
const NAME_MIN: f32 = 180.0;
/// The columns' side padding, both sides: the list's 4 and a row's 8.
const SIDES: f32 = 24.0;
/// Most lines Quick Look draws of a text file.
const PREVIEW_LINES: usize = 600;

fn muted(text: impl Into<SharedString>) -> gpui::Div {
    div().text_color(rgb(theme::muted())).child(text.into())
}

fn centered(child: impl IntoElement) -> AnyElement {
    div().size_full().flex().flex_col().items_center().justify_center().gap(px(6.0)).p(px(24.0)).child(child).into_any_element()
}

/// Which columns fit `width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Columns {
    date: bool,
    size: bool,
    kind: bool,
    folder: bool,
}

impl Columns {
    /// The columns that fit beside a name of at least [`NAME_MIN`], taken
    /// in order (Size, a search's Folder, Date Modified, Kind) until one
    /// doesn't: as the pane narrows Kind goes first, then Date Modified,
    /// like Finder's.
    fn for_width(width: f32, searching: bool) -> Self {
        let mut room = width - SIDES - NAME_MIN;
        let mut fits = |w: f32| {
            let fit = room >= w;
            if fit {
                room -= w;
            }
            fit
        };
        let size = fits(SIZE_W);
        let folder = size && searching && fits(FOLDER_W);
        let date = size && folder == searching && fits(DATE_W);
        let kind = date && fits(KIND_W);
        Self { date, size, kind, folder }
    }

    /// Their widths together.
    #[cfg(test)]
    fn width(self) -> f32 {
        [(self.date, DATE_W), (self.size, SIZE_W), (self.kind, KIND_W), (self.folder, FOLDER_W)].iter().filter(|(on, _)| *on).map(|(_, w)| w).sum()
    }
}

/// What a menu entry does.
type Run = Rc<dyn Fn(&mut FilesPane, &mut Window, &mut Context<FilesPane>)>;

/// One entry of the context menu.
enum Item {
    Action { id: &'static str, label: &'static str, keys: &'static str, run: Run },
    Separator,
}

fn action(
    id: &'static str,
    label: &'static str,
    keys: &'static str,
    run: impl Fn(&mut FilesPane, &mut Window, &mut Context<FilesPane>) + 'static,
) -> Item {
    Item::Action { id, label, keys, run: Rc::new(run) }
}

impl gpui::Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(5.0))
            .glass(7.0)
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .child(crate::assets::icon(self.icon, 13.0, theme::muted()))
            .child(self.label.clone())
    }
}

impl FilesPane {
    /// Browse | Edit, in the header (hosts with `files_ops`).
    pub(super) fn view_switch(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        let browsing = self.state.view == FilesView::Browse;
        let option = |id: &'static str, label: &'static str, on: bool, view: FilesView| {
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
            .on_click(cx.listener(move |this, _, window, cx| this.set_view(view, window, cx)))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(1.0))
            .mr(px(4.0))
            .child(option("files-view-browse", "Browse", browsing, FilesView::Browse))
            .child(option("files-view-edit", "Edit", !browsing, FilesView::Edit))
            .into_any_element()
    }

    fn tool(&self, id: &'static str, glyph: &'static str, tip: &'static str, enabled: bool) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(24.0))
            .continuous(6.0)
            .when(enabled, |el| el.cursor_pointer().hover(|s| s.bg(theme::hover_fill())))
            .when(!enabled, |el| el.opacity(0.35))
            .child(crate::assets::icon(glyph, 13.0, theme::muted()))
            .tooltip(crate::tooltip::Tooltip::text(tip))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let crumbs = browse::crumbs(&self.state.root, &self.state.browse);
        let last = crumbs.len() - 1;
        let path_bar = div().flex().flex_1().min_w_0().items_center().gap(px(2.0)).overflow_hidden().children(crumbs.into_iter().enumerate().map(
            |(i, (label, path))| {
                let here = i == last;
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(2.0))
                    .when(i > 0, |el| el.child(crate::assets::icon("chevron-right", 9.0, theme::muted())))
                    .child(
                        div()
                            .id(("files-crumb", i))
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .px(px(5.0))
                            .h(px(22.0))
                            .continuous(5.0)
                            .max_w(px(180.0))
                            .when(i == 0, |el| el.child(crate::assets::icon("folder", 12.0, theme::muted())))
                            .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label))
                            .text_color(rgb(if here { theme::foreground() } else { theme::muted() }))
                            .when(!here, |el| el.cursor_pointer().hover(|s| s.bg(theme::hover_fill())))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| this.navigate(path.clone(), true, cx)))
                            // A crumb takes drops too: into that folder.
                            .drag_over::<DraggedFiles>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.18)))
                            .on_drop(cx.listener(move |this, dragged: &DraggedFiles, window, cx| {
                                cx.stop_propagation();
                                let to = this_crumb(this, i);
                                this.drop_files(dragged, &to, window.modifiers().alt, cx);
                            })),
                    )
            },
        ));
        let (back, forward) = (!self.browser.history.back.is_empty(), !self.browser.history.forward.is_empty());
        let hidden = self.browser.hidden;
        let folders_first = self.browser.sort.folders_first;
        let search = match &self.browser.search {
            Some(search) => div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.0))
                .w(px(200.0))
                .h(px(24.0))
                .px(px(8.0))
                .continuous(6.0)
                .bg(theme::well_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .child(crate::assets::icon("search", 11.0, theme::muted()))
                .child(div().flex_1().min_w_0().child(search.field.clone()))
                .child(
                    div()
                        .id("files-search-clear")
                        .cursor_pointer()
                        .child(crate::assets::icon("close", 9.0, theme::muted()))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_search(cx);
                            window.focus(&this.browser.focus, cx);
                        })),
                )
                .into_any_element(),
            None => self
                .tool("files-search", "search", "Search  ⌘F", true)
                .on_click(cx.listener(|this, _, window, cx| this.open_browse_search(window, cx)))
                .into_any_element(),
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(36.0))
            .px(px(8.0))
            .border_t_1()
            .border_b_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(self.tool("files-back", "back", "Back  ⌘[", back).on_click(cx.listener(|this, _, _, cx| this.go_back(cx))))
            .child(self.tool("files-forward", "forward", "Forward  ⌘]", forward).on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))))
            .child(
                self.tool("files-up", "arrow-up", "Enclosing Folder  ⌘↑", !self.state.browse.is_empty() || self.browser.searching())
                    .on_click(cx.listener(|this, _, _, cx| this.go_parent(cx))),
            )
            .child(div().w(px(4.0)))
            .child(path_bar)
            .child(
                self.tool(
                    "files-hidden",
                    if hidden { "eye" } else { "eye-off" },
                    if hidden { "Hide Hidden Files  ⇧⌘." } else { "Show Hidden Files  ⇧⌘." },
                    true,
                )
                .when(hidden, |el| el.bg(theme::row_fill()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
            )
            // Folders on top: here, never over a column's label.
            .child(
                self.tool("files-folders-first", "folder", if folders_first { "Folders on Top: On" } else { "Folders on Top: Off" }, true)
                    .when(folders_first, |el| el.bg(theme::row_fill()))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_folders_first(cx))),
            )
            .child(self.tool("files-new", "plus", "New Folder ⇧⌘N, New File", true).on_click(cx.listener(|this, e: &gpui::ClickEvent, _, cx| {
                let at = e.position();
                this.browser.menu = Some(super::browser::Menu { at, target: None });
                cx.notify();
            })))
            .child(search)
            .into_any_element()
    }

    fn columns_header(&self, columns: Columns, cx: &mut Context<Self>) -> AnyElement {
        let sort = self.browser.sort;
        let head = |id: &'static str, label: &'static str, key: Option<SortKey>| {
            let on = key == Some(sort.key);
            let arrow = on.then(|| {
                let icon = crate::assets::icon("chevron-up", 9.0, theme::muted());
                if sort.ascending { icon } else { icon.with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::PI))) }
            });
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(4.0))
                .h_full()
                .px(px(6.0))
                .when(on, |el| el.text_color(rgb(theme::foreground())))
                .when(key.is_some(), |el| el.cursor_pointer().hover(|s| s.bg(theme::hover_fill())))
                .child(label)
                .children(arrow)
                .when_some(key, |el, key| el.on_click(cx.listener(move |this, _, _, cx| this.set_sort(key, cx))))
        };
        // The list's 4 and a row's 8: the labels sit over their columns.
        div()
            .flex_none()
            .flex()
            .items_center()
            .h(px(24.0))
            .px(px(SIDES / 2.0))
            .border_b_1()
            .border_color(rgb(theme::border()))
            .text_size(px(11.5))
            .text_color(rgb(theme::muted()))
            .overflow_hidden()
            .child(head("files-col-name", "Name", Some(SortKey::Name)).flex_1().min_w_0().pl(px(26.0)))
            .when(columns.folder, |el| el.child(head("files-col-folder", "Folder", None).w(px(FOLDER_W)).flex_none()))
            .when(columns.date, |el| el.child(head("files-col-date", "Date Modified", Some(SortKey::Modified)).w(px(DATE_W)).flex_none()))
            .when(columns.size, |el| el.child(head("files-col-size", "Size", Some(SortKey::Size)).w(px(SIZE_W)).flex_none().justify_end()))
            .when(columns.kind, |el| el.child(head("files-col-kind", "Kind", Some(SortKey::Kind)).w(px(KIND_W)).flex_none()))
            .into_any_element()
    }

    fn render_list(&mut self, rows: Rc<Vec<Row>>, columns: Columns, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let count = rows.len();
        let pane = cx.entity();
        let focused = self.browser.focus.is_focused(window);
        let picked: HashSet<String> = self.browser.picked.paths.iter().cloned().collect();
        let picked_rc: Rc<Vec<String>> = Rc::new(self.browser.picked.ordered(&rows));
        let rename = self.browser.rename.as_ref().map(|r| (r.path.clone(), r.field.clone()));
        let now = chrono::Local::now();
        // The drag's own fields, from this frame (see `drag_of`).
        let template = self.drag_of("", &picked_rc);
        let local = template.local_root.is_some();
        gpui::uniform_list("files-browse", count, move |range, _, _| {
            range
                .map(|ix| {
                    let row = rows[ix].clone();
                    let selected = picked.contains(&row.path);
                    let fill = if selected && focused {
                        Some(rgb(theme::selection()))
                    } else if selected {
                        Some(theme::row_fill())
                    } else {
                        None
                    };
                    let renaming = rename.as_ref().filter(|(p, _)| *p == row.path).map(|(_, f)| f.clone());
                    #[allow(clippy::cast_precision_loss)]
                    let indent = row.depth as f32 * INDENT;
                    let disclosure = row.dir && row.folder.is_none();
                    let chevron = disclosure.then(|| {
                        let icon = crate::assets::icon("chevron-right", 9.0, theme::muted());
                        let icon = if row.expanded {
                            icon.with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::FRAC_PI_2)))
                        } else {
                            icon
                        };
                        let (pane, path) = (pane.clone(), row.path.clone());
                        div()
                            .id(("files-disclose", ix))
                            .size(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .child(icon)
                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                cx.stop_propagation();
                                pane.update(cx, |this, cx| this.toggle_folder(&path, cx));
                            })
                    });
                    let name: AnyElement = match renaming {
                        Some(field) => div()
                            .flex_1()
                            .min_w_0()
                            .px(px(4.0))
                            .continuous(4.0)
                            .bg(theme::well_fill())
                            .border_1()
                            .border_color(rgb(theme::accent()))
                            .child(field)
                            .into_any_element(),
                        None => div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .when(row.link, |el| el.italic())
                            .child(row.name.clone())
                            .into_any_element(),
                    };
                    let drag = DraggedFiles { paths: if selected { picked_rc.clone() } else { Rc::new(vec![row.path.clone()]) }, ..template.clone() };
                    let preview = DragPreview { label: drag.label(), icon: if drag.paths.len() == 1 { row.icon() } else { "layers" } };
                    let cell = |w: f32| div().flex_none().w(px(w)).px(px(6.0)).overflow_hidden().text_ellipsis().whitespace_nowrap();
                    let (down, click, right) = (pane.clone(), pane.clone(), pane.clone());
                    let (row_down, row_click, row_right) = (row.clone(), row.clone(), row.clone());
                    let mut el = div()
                        .id(("files-browse-row", ix))
                        .h(px(ROW))
                        .w_full()
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .continuous(5.0)
                        .when_some(fill, |el, f| el.bg(f))
                        .when(fill.is_none(), |el| el.hover(|s| s.bg(theme::hover_fill())))
                        .when(row.hidden, |el| el.opacity(0.6))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(5.0))
                                .pl(px(indent))
                                .child(div().w(px(16.0)).flex_none().flex().items_center().children(chevron))
                                .child(crate::assets::icon(row.icon(), 14.0, if row.dir { theme::accent() } else { theme::muted() }))
                                .child(name),
                        )
                        .when(columns.folder, |el| {
                            let folder = row.folder.clone().unwrap_or_default();
                            el.child(cell(FOLDER_W).text_color(rgb(theme::muted())).child(if folder.is_empty() { "—".to_owned() } else { folder }))
                        })
                        .when(columns.date, |el| el.child(cell(DATE_W).text_color(rgb(theme::muted())).child(browse::date_label(row.mtime, now))))
                        .when(columns.size, |el| {
                            el.child(cell(SIZE_W).flex().justify_end().text_color(rgb(theme::muted())).child(browse::size_label(&row)))
                        })
                        .when(columns.kind, |el| el.child(cell(KIND_W).text_color(rgb(theme::muted())).child(row.kind)))
                        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let m = e.modifiers;
                            down.update(cx, |this, cx| {
                                if e.click_count >= 2 && !m.platform && !m.shift {
                                    return this.open_row(&row_down, window, cx);
                                }
                                // A press on a selected row keeps the selection (to drag it); the click decides.
                                if !this.browser.picked.contains(&row_down.path) || m.platform || m.shift {
                                    this.browse_click(ix, m.platform, m.shift, window, cx);
                                } else {
                                    this.commit_rename(cx);
                                    window.focus(&this.browser.focus, cx);
                                }
                            });
                        })
                        .on_click(move |e: &gpui::ClickEvent, window, cx| {
                            let m = e.modifiers();
                            if m.platform || m.shift || e.click_count() > 1 {
                                return;
                            }
                            click.update(cx, |this, cx| {
                                if this.browser.picked.paths.len() > 1 && this.browser.picked.contains(&row_click.path) {
                                    this.browse_click(ix, false, false, window, cx);
                                }
                            });
                        })
                        .on_mouse_down(MouseButton::Right, move |e: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            right.update(cx, |this, cx| {
                                if !this.browser.picked.contains(&row_right.path) {
                                    this.browse_click(ix, false, false, window, cx);
                                }
                                this.browser.menu = Some(super::browser::Menu { at: e.position, target: Some(row_right.path.clone()) });
                                cx.notify();
                            });
                        });
                    // Local files on macOS: the native drag from the first move (the system's file icon and
                    // badge throughout, like Finder), still dropped back onto our own rows. Elsewhere, or
                    // remote: the in-app preview, promoted for local files when it leaves the window.
                    let native = local && cfg!(target_os = "macos");
                    el = if native {
                        el.on_native_drag(drag, |d: &DraggedFiles, _, _, _| d.external())
                    } else {
                        el.on_drag(drag, move |_, _, _, cx| {
                            let preview = DragPreview { label: preview.label.clone(), icon: preview.icon };
                            cx.new(|_| preview)
                        })
                    };
                    if local && !native {
                        el = el.external_drag_payload(|d: &DraggedFiles, _, _| d.external());
                    }
                    if row.dir {
                        let (moves, uploads) = (pane.clone(), pane.clone());
                        let (to_move, to_upload) = (row.path.clone(), row.path.clone());
                        let target = row.path.clone();
                        el =
                            el.drag_over::<DraggedFiles>(move |s, d, _, _| {
                                if browse::can_drop(&d.paths, &target) { s.bg(rgb(theme::accent()).opacity(0.22)) } else { s }
                            })
                            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.22)))
                            .on_drop(move |d: &DraggedFiles, window, cx| {
                                cx.stop_propagation();
                                let copy = window.modifiers().alt;
                                moves.update(cx, |this, cx| this.drop_files(d, &to_move, copy, cx));
                            })
                            .on_drop(move |paths: &ExternalPaths, _, cx| {
                                cx.stop_propagation();
                                let paths = paths.paths().to_vec();
                                uploads.update(cx, |this, cx| this.upload_into(paths, to_upload.clone(), cx));
                            });
                    }
                    el
                })
                .collect()
        })
        .track_scroll(&self.browser.scroll)
        .size_full()
        .into_any_element()
    }

    pub(super) fn render_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(path) = self.browser.rename_next.take() {
            cx.defer_in(window, move |this, window, cx| this.start_rename(path, window, cx));
        }
        let rows = self.browse_rows();
        let width = self.body_bounds.get().map_or(800.0, |b| f32::from(b.size.width));
        let searching = self.browser.searching();
        let columns = Columns::for_width(width, searching);
        let dir = self.state.browse.clone();
        let error = (!searching).then(|| self.list_errors.get(&dir).cloned()).flatten();
        let loading = !searching && !self.listings.contains_key(&dir) && error.is_none();
        let body = match (&error, loading) {
            (Some(error), _) => {
                centered(div().flex().flex_col().items_center().gap(px(4.0)).child("Couldn't open this folder").child(muted(error.clone())))
            }
            (None, true) => centered(muted("Loading…")),
            _ if rows.is_empty() && searching => {
                let busy = self.browser.search.as_ref().is_some_and(|s| s.searching);
                centered(muted(if busy { "Searching…" } else { "No items match" }))
            }
            _ if rows.is_empty() => centered(muted(if self.browser.hidden { "Empty folder" } else { "No visible items" })),
            _ => self.render_list(rows.clone(), columns, window, cx),
        };
        let (n, m) = (rows.len(), self.browser.picked.paths.len());
        let mut status = if m > 0 { format!("{m} of {n} selected") } else { format!("{n} item{}", if n == 1 { "" } else { "s" }) };
        if let Some(search) = self.browser.search.as_ref().filter(|_| searching) {
            if search.searching {
                status.push_str(" · searching…");
            } else if search.truncated {
                status.push_str(" · more matches not shown");
            }
        }
        let notice = self.notice.clone().map(|notice| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(28.0))
                .px(px(10.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .text_size(px(12.0))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().text_color(rgb(theme::warning())).child(notice))
                .child(div().id("files-browse-notice-close").cursor_pointer().child(crate::assets::icon("close", 10.0, theme::muted())).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    }),
                ))
        });
        let shown = dir.clone();
        div()
            .id("files-browser")
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.browser.focus)
            .text_size(px(12.5))
            .child(self.toolbar(cx))
            .children(notice)
            .child(self.columns_header(columns, cx))
            .child(
                div()
                    .id("files-browse-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .px(px(4.0))
                    .py(px(2.0))
                    .child(body)
                    // The folder's background: a press clears the selection; drops go into the folder shown.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.commit_rename(cx);
                            this.browser.picked.clear();
                            this.browser.menu = None;
                            window.focus(&this.browser.focus, cx);
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, e: &MouseDownEvent, window, cx| {
                            this.browser.picked.clear();
                            this.browser.menu = Some(super::browser::Menu { at: e.position, target: None });
                            window.focus(&this.browser.focus, cx);
                            cx.notify();
                        }),
                    )
                    .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.06)).border_2().border_color(rgb(theme::accent())))
                    .drag_over::<DraggedFiles>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.04)))
                    .on_drop(cx.listener({
                        let shown = shown.clone();
                        move |this, paths: &ExternalPaths, window, cx| {
                            window.focus(&this.browser.focus, cx);
                            this.upload_into(paths.paths().to_vec(), shown.clone(), cx);
                        }
                    }))
                    .on_drop(cx.listener(move |this, dragged: &DraggedFiles, window, cx| {
                        if !this.browser.searching() {
                            this.drop_files(dragged, &shown, window.modifiers().alt, cx);
                        }
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(22.0))
                    .border_t_1()
                    .border_color(rgb(theme::border()))
                    .text_size(px(11.0))
                    .text_color(rgb(theme::muted()))
                    .child(status),
            )
            .into_any_element()
    }

    /// The context menu (or the + menu), above everything.
    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        let menu = self.browser.menu.as_ref()?;
        let target = menu.target.clone();
        let mut items = Vec::new();
        if let Some(target) = target {
            let picked = self.browser.picked.paths.len();
            let one = picked <= 1;
            items.push(action("open", "Open", "⌘↓", |this, window, cx| this.open_selected(window, cx)));
            items.push(action("quicklook", "Quick Look", "Space", |this, _, cx| this.toggle_preview(cx)));
            items.push(Item::Separator);
            if one {
                let path = target.clone();
                items.push(action("rename", "Rename", "↩", move |this, window, cx| this.start_rename(path.clone(), window, cx)));
            }
            items.push(action("duplicate", "Duplicate", "⌘D", |this, _, cx| this.duplicate_selected(cx)));
            items.push(action("copy", "Copy", "⌘C", |this, _, cx| this.copy_selected(cx)));
            items.push(action("paste", "Paste Items", "⌘V", |this, _, cx| this.paste_files(cx)));
            items.push(Item::Separator);
            items.push(action("copy-path", "Copy Path", "⌥⌘C", |this, _, cx| {
                let paths = this.picked_paths();
                this.copy_paths(&paths, cx);
            }));
            if self.local {
                let path = target.clone();
                items.push(action("reveal", "Reveal in Finder", "", move |this, _, _| this.reveal_in_finder(&path)));
            }
            let path = target.clone();
            items.push(action("terminal", "Open in Terminal", "", move |this, _, cx| this.open_terminal_at(Some(&path), cx)));
            items.push(Item::Separator);
            items.push(action("trash", "Move to Trash", "⌘⌫", |this, _, cx| this.trash_selected(cx)));
        } else {
            items.push(action("new-folder", "New Folder", "⇧⌘N", |this, window, cx| this.new_folder(window, cx)));
            items.push(action("new-file", "New File", "", |this, window, cx| this.new_file(window, cx)));
            items.push(Item::Separator);
            items.push(action("paste", "Paste Items", "⌘V", |this, _, cx| this.paste_files(cx)));
            items.push(action("copy-path", "Copy Path", "⌥⌘C", |this, _, cx| {
                let dir = vec![this.state.browse.clone()];
                this.copy_paths(&dir, cx);
            }));
            if self.local {
                let dir = self.state.browse.clone();
                items.push(action("reveal", "Reveal in Finder", "", move |this, _, _| this.reveal_in_finder(&dir)));
            }
            items.push(action("terminal", "Open in Terminal", "", |this, _, cx| this.open_terminal_at(None, cx)));
            items.push(Item::Separator);
            let hidden = self.browser.hidden;
            items.push(action("hidden", if hidden { "Hide Hidden Files" } else { "Show Hidden Files" }, "⇧⌘.", |this, _, cx| {
                this.toggle_hidden(cx)
            }));
        }
        let rows = items.into_iter().map(|item| match item {
            Item::Separator => div().h(px(1.0)).my(px(4.0)).mx(px(6.0)).bg(crate::surface::hairline()).into_any_element(),
            Item::Action { id, label, keys, run } => div()
                .id(id)
                .flex()
                .items_center()
                .justify_between()
                .gap(px(18.0))
                .h(px(24.0))
                .px(px(10.0))
                .continuous(5.0)
                .cursor_pointer()
                .hover(|s| s.bg(theme::row_fill()))
                .child(label)
                .child(muted(keys).text_size(px(11.5)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.browser.menu = None;
                    run(this, window, cx);
                    cx.notify();
                }))
                .into_any_element(),
        });
        let card = div()
            .id("files-menu")
            .occlude()
            .min_w(px(210.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .glass(8.0)
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .children(rows)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.browser.menu = None;
                cx.notify();
            }));
        Some(deferred(anchored().position(menu.at).snap_to_window_with_margin(px(8.0)).child(card)).with_priority(1).into_any_element())
    }

    /// Quick Look: the selected item over the list (Space or Esc closes it).
    pub(super) fn render_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.browser.preview.as_ref()?;
        let body: AnyElement = match &preview.content {
            Content::Loading => centered(muted("Loading…")),
            Content::Failed(error) => centered(muted(error.clone())),
            Content::Binary { mime, size } if *size == 0 && mime.starts_with("Folder") => centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .child(crate::assets::icon("folder", 48.0, theme::accent()))
                    .child(muted(mime.clone())),
            ),
            Content::Binary { mime, size } => centered(muted(model::binary_summary(mime, *size))),
            Content::Image { image, .. } => div()
                .size_full()
                .p(px(12.0))
                .flex()
                .items_center()
                .justify_center()
                .child(img(image.clone()).size_full().object_fit(ObjectFit::ScaleDown))
                .into_any_element(),
            Content::Text(file) => {
                let lines = file.doc.line_count().min(PREVIEW_LINES);
                let more = file.doc.line_count() > PREVIEW_LINES || file.truncated;
                div()
                    .id("files-preview-text")
                    .size_full()
                    .overflow_y_scroll()
                    .p(px(12.0))
                    .font_family(theme::prefs().font_family.clone())
                    .text_size(px(12.0))
                    .children((0..lines).map(|i| div().whitespace_nowrap().child(file.doc.line(i).replace('\t', "    "))))
                    .when(more, |el| el.child(muted("…")))
                    .into_any_element()
            }
        };
        let name = model::name(&preview.path).to_owned();
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("files-preview")
                        .occlude()
                        .w(gpui::relative(0.8))
                        .h(gpui::relative(0.78))
                        .max_w(px(900.0))
                        .flex()
                        .flex_col()
                        .continuous(12.0)
                        .bg(rgb(theme::popover()))
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .shadow_xl()
                        .overflow_hidden()
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .h(px(30.0))
                                .px(px(10.0))
                                .border_b_1()
                                .border_color(rgb(theme::border()))
                                .text_size(px(12.5))
                                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().child(name))
                                .child(
                                    div()
                                        .id("files-preview-close")
                                        .cursor_pointer()
                                        .child(crate::assets::icon("close", 10.0, theme::muted()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.browser.preview = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(div().flex_1().min_h_0().child(body)),
                )
                .into_any_element(),
        )
    }

    /// The upload's progress and Cancel, in either view.
    pub(super) fn render_transfer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        let transfer = self.browser.transfer.as_ref()?;
        let (sent, total) =
            (transfer.progress.0.load(std::sync::atomic::Ordering::Relaxed), transfer.progress.1.load(std::sync::atomic::Ordering::Relaxed));
        let percent = sent.min(total).saturating_mul(100).checked_div(total).unwrap_or(0);
        Some(
            div()
                .id("files-upload")
                .occlude()
                .absolute()
                .bottom(px(30.0))
                .right(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(12.0))
                .py(px(8.0))
                .glass(8.0)
                .text_size(px(12.0))
                .child(format!("{} · {percent}%", transfer.label))
                .child(
                    div()
                        .id("files-upload-cancel")
                        .text_color(rgb(theme::muted()))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::foreground())))
                        .child("Cancel")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_upload(cx))),
                )
                .into_any_element(),
        )
    }

    /// The browser's keys; true when handled. Bound keys (⌘C ⌘V ⌘A ⌘D ⌘[
    /// ⌘] ⌘↑ ⌘↓ ⇧⌘N) arrive as actions instead (`browse_action`).
    pub(super) fn browse_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let k = &e.keystroke;
        let m = k.modifiers;
        let key = k.key.as_str();
        // Anywhere in the block.
        match key {
            "f" if m.platform && !m.shift && !m.alt && !m.control => {
                self.open_browse_search(window, cx);
                return true;
            }
            "." | ">" if m.platform && m.shift => {
                self.toggle_hidden(cx);
                return true;
            }
            "c" if m.platform && m.alt => {
                let paths = self.picked_paths();
                self.copy_paths(&paths, cx);
                return true;
            }
            _ => {}
        }
        if !self.browser.focus.is_focused(window) {
            return false;
        }
        match key {
            "escape" if self.browser.menu.is_some() => self.browser.menu = None,
            "escape" if self.browser.preview.is_some() => self.browser.preview = None,
            "escape" if self.browser.search.is_some() => self.clear_search(cx),
            "escape" => self.browser.picked.clear(),
            "up" | "down" if !m.platform && !m.control && !m.alt => self.browse_step(if key == "up" { -1 } else { 1 }, m.shift, cx),
            "right" | "left" if !m.control && !m.alt => self.browse_disclose(key == "right", cx),
            "enter" if !m.platform => self.rename_selected(window, cx),
            "enter" | "o" if m.platform => self.open_selected(window, cx),
            "space" if !m.platform && !m.control && !m.alt => self.toggle_preview(cx),
            "backspace" | "delete" if m.platform => self.trash_selected(cx),
            _ if !m.platform && !m.control && !m.alt && !m.function => {
                let text = k.key_char.clone().or_else(|| (key.chars().count() == 1).then(|| key.to_owned()));
                let Some(text) = text.filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control())) else { return false };
                self.type_to_select(&text, cx);
            }
            _ => return false,
        }
        cx.notify();
        true
    }
}

/// Crumb `i`'s folder (the path bar of the folder shown).
fn this_crumb(this: &FilesPane, i: usize) -> String {
    browse::crumbs(&this.state.root, &this.state.browse).get(i).map(|(_, p)| p.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_drop_kind_then_date_as_the_pane_narrows() {
        let all = Columns { date: true, size: true, kind: true, folder: false };
        assert_eq!(Columns::for_width(1000.0, false), all);
        let no_kind = Columns { kind: false, ..all };
        assert_eq!(Columns::for_width(560.0, false), no_kind, "a half-width pane");
        assert_eq!(Columns::for_width(380.0, false), Columns { date: false, ..no_kind });
        assert_eq!(Columns::for_width(250.0, false), Columns { date: false, size: false, kind: false, folder: false });
        // Searching: the Folder column before Date Modified.
        assert_eq!(Columns::for_width(560.0, true), Columns { date: false, size: true, kind: false, folder: true });
        assert_eq!(Columns::for_width(1200.0, true), Columns { folder: true, ..all });
        for searching in [false, true] {
            for width in (150..1600).step_by(10) {
                #[allow(clippy::cast_precision_loss)]
                let width = width as f32;
                let c = Columns::for_width(width, searching);
                assert!(c.width() == 0.0 || SIDES + NAME_MIN + c.width() <= width, "{width}: {c:?} overflows");
                assert!(!c.kind || c.date, "{width}: Kind only with Date Modified");
                assert!(!c.date || c.size, "{width}: Date Modified only with Size");
                assert!(!c.folder || searching);
            }
        }
    }
}
