//! Drawing the Files block: its header (tabs, the Rendered/Source switch,
//! find and the tree toggle), the viewer, the tree and the ⇧⌘P finder.

use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, ExternalPaths, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ObjectFit, Render,
    SharedString, StyledImage as _, Window, div, img, prelude::*, px, rgb,
};
use silo_proto::model::BlockInfo;

use std::rc::Rc;

use super::{
    Content, Drag, FilesPane, browse,
    browser::{DragPreview, DraggedFiles},
    model,
};
use crate::{
    commands::act,
    pane_header::{BUTTON, BlockHeader, HeaderParts, HeaderStyle, button, keep_press, labels},
    theme::{self, Continuous as _},
};

/// A tree row's height.
const ROW: f32 = 22.0;
/// Indent per folder level.
const INDENT: f32 = 12.0;

fn hint(text: impl Into<SharedString>) -> gpui::Div {
    div().text_color(rgb(theme::muted())).child(text.into())
}

fn centered(child: impl IntoElement) -> AnyElement {
    div().size_full().flex().flex_col().items_center().justify_center().gap(px(6.0)).p(px(24.0)).child(child).into_any_element()
}

impl BlockHeader for FilesPane {
    fn header(&mut self, style: HeaderStyle, _: Option<&BlockInfo>, cx: &mut Context<Self>) -> HeaderParts {
        let root = self.state.root.clone();
        let name = model::name(&root).to_owned();
        let name = if name.is_empty() { root.clone() } else { name };
        let browsing = self.browsing();
        let mut parts = if self.state.open.is_empty() || browsing {
            labels(name.clone(), Some(tilde(&root)), style)
        } else {
            HeaderParts::new(self.tabs(style, cx), name.clone())
        };
        if browsing {
            parts.actions = vec![self.view_switch(style, cx)];
            return parts;
        }
        let active_md = self.active().is_some_and(|a| matches!(self.contents.get(a), Some(Content::Text(f)) if f.shown == model::Shown::Markdown));
        let mut actions = Vec::new();
        if self.ops() {
            actions.push(self.view_switch(style, cx));
        }
        if active_md {
            actions.push(self.source_switch(style, cx));
        }
        actions.push(
            keep_press(button("files-find", "search", "Find a File  ⇧⌘P", style.palette.icon, true, style))
                .on_click(cx.listener(|this, _, window, cx| this.open_finder(window, cx)))
                .into_any_element(),
        );
        let tree = self.state.tree;
        actions.push(
            keep_press(button(
                "files-tree",
                "sidebar-right",
                if tree { "Hide Tree  ⇧⌘E" } else { "Show Tree  ⇧⌘E" },
                style.palette.icon,
                true,
                style,
            ))
            .when(tree, |el| el.bg(style.palette.selected()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_tree(cx)))
            .into_any_element(),
        );
        parts.actions = actions;
        parts
    }
}

impl FilesPane {
    /// The open files as tabs: name (and folder for twins), a dot when
    /// changed on disk, × to close; middle-click closes too.
    fn tabs(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let labels = model::tab_labels(&self.state.open);
        let active = self.state.active.as_deref();
        let tabs = self
            .state
            .open
            .iter()
            .zip(labels)
            .map(|(path, (name, dir))| crate::header_tabs::Tab {
                name,
                dir,
                tooltip: path.clone(),
                shown: active == Some(path.as_str()),
                dirty: self.dirty(path),
                dot: self.changed.contains(path),
            })
            .collect();
        let paths = Rc::new(self.state.open.clone());
        let (pane, closing) = (cx.entity().downgrade(), cx.entity().downgrade());
        let (open, close) = (paths.clone(), paths);
        crate::header_tabs::strip(
            crate::header_tabs::Ids { strip: "files-tabs", tab: "files-tab", close: "files-tab-close" },
            tabs,
            style,
            move |i, _, cx| {
                let _ = pane.update(cx, |this, cx| this.open(&open[i], true, cx));
            },
            move |i, _, cx| {
                let _ = closing.update(cx, |this, cx| this.close(&close[i], cx));
            },
        )
    }

    /// Rendered | Source, for a markdown tab.
    fn source_switch(&self, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        let source = self.active().is_some_and(|a| self.source.contains(a));
        let option = |id: &'static str, label: &'static str, on: bool| {
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
            .on_click(cx.listener(move |this, _, _, cx| {
                if !on {
                    this.toggle_source(cx);
                }
            }))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(1.0))
            .mr(px(4.0))
            .child(option("files-rendered", "Rendered", !source))
            .child(option("files-source", "Source", source))
            .into_any_element()
    }

    fn render_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(path) = self.state.active.clone() else {
            let hint = if self.state.tree { "Pick a file in the tree, or press ⇧⌘P to find one" } else { "Press ⇧⌘P to find a file" };
            return centered(div().flex().flex_col().items_center().gap(px(4.0)).child("No file open").child(hint_text(hint)));
        };
        let mut content = self.contents.remove(&path);
        let body = match &mut content {
            None | Some(Content::Loading) => centered(hint("Loading…")),
            Some(Content::Failed(error)) => centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .child(format!("Couldn't open {}", model::name(&path)))
                    .child(hint_text(error.clone())),
            ),
            Some(Content::Binary { mime, size }) => centered(hint(model::binary_summary(mime, *size))),
            Some(Content::Image { image, mime, size, dims }) => {
                let kind = mime.trim_start_matches("image/").trim_end_matches("+xml").to_uppercase();
                let caption = match dims {
                    Some((w, h)) => format!("{kind} · {w}×{h} · {}", model::human_size(*size)),
                    None => format!("{kind} · {}", model::human_size(*size)),
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .p(px(16.0))
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(img(image.clone()).size_full().object_fit(ObjectFit::ScaleDown)),
                    )
                    .child(div().flex_none().flex().justify_center().child(hint(caption)))
                    .into_any_element()
            }
            Some(Content::Text(file)) => {
                if file.markdown_stale && !self.source.contains(&path) {
                    file.markdown_stale = false;
                    if let Some(md) = file.markdown.as_mut() {
                        md.set_source(&file.doc.text);
                    }
                }
                let rendered = file.markdown.as_ref().filter(|_| !self.source.contains(&path));
                let note = file.truncated.then(|| self.truncated_note(&path, file.doc.text.len() as u64, file.size, cx));
                let main = match rendered {
                    Some(md) => md.render(24.0),
                    None => self.render_text(&path, file, window, cx),
                };
                div().size_full().flex().flex_col().children(note).child(div().flex_1().min_h_0().child(main)).into_any_element()
            }
        };
        if let Some(content) = content {
            self.contents.insert(path, content);
        }
        body
    }

    fn truncated_note(&self, path: &str, shown: u64, size: u64, cx: &mut Context<Self>) -> AnyElement {
        let more = size > silo_proto::control::FileRead::MAX_BYTES;
        let path = path.to_owned();
        let label = if more { format!("Load {}", model::human_size(silo_proto::control::FileRead::MAX_BYTES)) } else { "Load all".to_owned() };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(5.0))
            .bg(rgb(theme::mix(theme::background(), theme::warning(), 0.12)))
            .text_size(px(12.0))
            .child(hint(format!("Showing the first {} of {}", model::human_size(shown), model::human_size(size))))
            .when(shown < silo_proto::control::FileRead::MAX_BYTES, |el| {
                el.child(
                    div()
                        .id("files-load-all")
                        .px(px(8.0))
                        .py(px(2.0))
                        .continuous(5.0)
                        .bg(theme::control_fill())
                        .hover(|s| s.bg(theme::hover_fill()))
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.read(path.clone(), Some(silo_proto::control::FileRead::MAX_BYTES), cx))),
                )
            })
            .into_any_element()
    }

    /// The find or go-to bar over the viewer, and a notice.
    fn bars(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let bar = || {
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
        };
        let mut out = Vec::new();
        out.extend(self.disk_bar(cx));
        if let Some(find) = &self.find {
            let count = match (find.current, find.matches.len()) {
                (_, 0) if find.field.read(cx).text.is_empty() => String::new(),
                (_, 0) => "No matches".to_owned(),
                (Some(c), n) => format!("{} of {n}", c + 1),
                (None, n) => format!("{n} matches"),
            };
            out.push(
                bar()
                    .child(crate::assets::icon("search", 12.0, theme::muted()))
                    .child(find.field.clone())
                    .child(hint(count))
                    .child(hint("↩ next · ↑ previous · esc"))
                    .into_any_element(),
            );
        }
        if let Some(goto) = &self.goto {
            out.push(bar().child(hint("Line")).child(goto.field.clone()).child(hint("↩ go · esc")).into_any_element());
        }
        if let Some(notice) = &self.notice {
            out.push(
                bar()
                    .child(div().flex_1().min_w_0().text_color(rgb(theme::warning())).child(notice.clone()))
                    .child(div().id("files-notice-close").cursor_pointer().child(crate::assets::icon("close", 10.0, theme::muted())).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        }),
                    ))
                    .into_any_element(),
            );
        }
        out
    }

    fn render_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree_rows();
        let focused = self.tree_focus.is_focused(window);
        let selected = self.tree_selected.clone();
        let active = self.state.active.clone();
        let root_error = self.list_errors.get("").cloned();
        let loading = !self.listings.contains_key("") && root_error.is_none();
        let pane = cx.entity();
        let count = rows.len();
        // Drags: a row alone (within the block, and out of the window for this Mac's files).
        let template = self.drag_of("", &Rc::new(Vec::new()));
        let (local, ops) = (template.local_root.is_some(), self.ops());
        let list = gpui::uniform_list("files-tree", count, move |range, _, _| {
            let rows = &rows;
            range
                .map(|ix| {
                    let row = rows[ix].clone();
                    let is_selected = selected.as_deref() == Some(row.path.as_str());
                    let is_active = active.as_deref() == Some(row.path.as_str());
                    let fill = if is_selected && focused {
                        Some(rgb(theme::selection()))
                    } else if is_selected || is_active {
                        Some(theme::row_fill())
                    } else {
                        None
                    };
                    let chevron = row.dir.then(|| {
                        let icon = crate::assets::icon("chevron-right", 10.0, theme::muted());
                        if row.expanded {
                            icon.with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::FRAC_PI_2)))
                        } else {
                            icon
                        }
                    });
                    let glyph = crate::assets::icon(if row.dir { "folder" } else { "file-text" }, 13.0, theme::muted());
                    let pane = pane.clone();
                    #[allow(clippy::cast_precision_loss)]
                    let indent = row.depth as f32 * INDENT;
                    div()
                        .id(("files-tree-row", ix))
                        .h(px(ROW))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .pl(px(8.0 + indent))
                        .pr(px(8.0))
                        .when_some(fill, |el, f| el.bg(f))
                        .when(fill.is_none(), |el| el.hover(|s| s.bg(theme::hover_fill())))
                        .when(row.hidden, |el| el.opacity(0.55))
                        .child(div().w(px(10.0)).flex_none().flex().items_center().children(chevron))
                        .child(glyph)
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .when(row.link, |el| el.italic())
                                .text_color(rgb(if is_active { theme::foreground() } else { theme::ink_on(theme::background()) }))
                                .child(row.name.clone()),
                        )
                        .map(|el| {
                            if !ops {
                                return el;
                            }
                            let drag = DraggedFiles { paths: Rc::new(vec![row.path.clone()]), ..template.clone() };
                            let preview = (drag.label(), if row.dir { "folder" } else { "file-text" });
                            // Local files on macOS: the native drag from the first move (browser_view.rs).
                            let el = if local && cfg!(target_os = "macos") {
                                el.on_native_drag(drag, |d: &DraggedFiles, _, _, _| d.external())
                            } else {
                                let el = el.on_drag(drag, move |_, _, _, cx| cx.new(|_| DragPreview { label: preview.0.clone(), icon: preview.1 }));
                                if local { el.external_drag_payload(|d: &DraggedFiles, _, _| d.external()) } else { el }
                            };
                            if !row.dir {
                                return el;
                            }
                            let (moves, uploads, target) = (pane.clone(), pane.clone(), row.path.clone());
                            let (to_move, to_upload) = (row.path.clone(), row.path.clone());
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
                            })
                        })
                        .on_click(move |e: &ClickEvent, window, cx| {
                            let behind = e.modifiers().platform;
                            pane.update(cx, |this, cx| this.tree_click(&row, behind, window, cx));
                        })
                })
                .collect()
        })
        .track_scroll(&self.tree_scroll)
        .size_full();
        let body: AnyElement = match (root_error, loading) {
            (Some(error), _) => centered(hint(error)),
            (None, true) => centered(hint("Loading…")),
            (None, false) if count == 0 => centered(hint("Empty folder")),
            _ => list.into_any_element(),
        };
        let width = self.tree_width;
        // The edge: drag to resize.
        let edge = div().id("files-tree-edge").absolute().top_0().left(px(-3.0)).w(px(6.0)).h_full().cursor_col_resize().on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.drag = Some(Drag::Tree { width: this.tree_width, x: f32::from(e.position.x) });
                cx.notify();
            }),
        );
        div()
            .id("files-tree-panel")
            .relative()
            .flex_none()
            .w(px(width))
            .h_full()
            .border_l_1()
            // Its top edge meets the block header: a rule there, as on its left.
            .border_t_1()
            .border_color(rgb(theme::border()))
            .track_focus(&self.tree_focus)
            .text_size(px(12.5))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| window.focus(&this.tree_focus, cx)))
            // Drops on the tree's background go into the root (files from Finder: uploaded, on hosts that take them).
            .when(self.ops(), |el| {
                el.drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.06)))
                    .drag_over::<DraggedFiles>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.04)))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.upload_into(paths.paths().to_vec(), String::new(), cx)))
                    .on_drop(cx.listener(|this, d: &DraggedFiles, window, cx| this.drop_files(d, "", window.modifiers().alt, cx)))
            })
            .child(div().size_full().py(px(4.0)).child(body))
            .child(edge)
            .into_any_element()
    }

    fn render_finder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let finder = self.finder.as_ref()?;
        let rows = finder.found.iter().take(60).enumerate().map(|(i, found)| {
            let selected = i == finder.selected;
            let name = model::name(&found.path);
            let base = found.path.len() - name.len();
            let style = gpui::HighlightStyle { color: Some(rgb(theme::accent()).into()), ..Default::default() };
            let in_name: Vec<_> = found.ranges.iter().filter(|r| r.start >= base).map(|r| (r.start - base..r.end - base, style)).collect();
            let in_dir: Vec<_> = found.ranges.iter().filter(|r| r.end <= base).map(|r| (r.clone(), style)).collect();
            let dir = found.path[..base].trim_end_matches('/').to_owned();
            div()
                .id(("files-finder-row", i))
                .flex()
                .items_baseline()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(4.0))
                .continuous(6.0)
                .when(selected, |el| el.bg(rgb(theme::row_active())))
                .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
                .cursor_pointer()
                .child(div().flex_none().child(gpui::StyledText::new(name.to_owned()).with_highlights(in_name)))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(11.5))
                        .text_color(rgb(theme::muted()))
                        .child(gpui::StyledText::new(dir).with_highlights(in_dir)),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.finder_pick(i, window, cx)))
        });
        let empty = finder.found.is_empty().then(|| div().px(px(10.0)).py(px(6.0)).child(hint("No files match")));
        let more = finder
            .truncated
            .then(|| div().px(px(10.0)).py(px(4.0)).text_size(px(11.0)).child(hint("Searched part of the folder: it holds too many files")));
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
                        .id("files-finder")
                        .w(px(520.0))
                        .max_w_full()
                        .mx(px(12.0))
                        .flex()
                        .flex_col()
                        .continuous(10.0)
                        .bg(rgb(theme::popover()))
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .shadow_lg()
                        .text_size(px(13.0))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .px(px(12.0))
                                .h(px(36.0))
                                .border_b_1()
                                .border_color(rgb(theme::border()))
                                .child(crate::assets::icon("search", 13.0, theme::muted()))
                                .child(finder.field.clone()),
                        )
                        .child(
                            div()
                                .id("files-finder-list")
                                .max_h(px(360.0))
                                .overflow_y_scroll()
                                .p(px(4.0))
                                .children(rows)
                                .children(empty)
                                .children(more),
                        ),
                )
                .into_any_element(),
        )
    }

    fn key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.browsing() && self.browse_key(e, window, cx) {
            cx.stop_propagation();
            return cx.notify();
        }
        let k = &e.keystroke;
        let m = k.modifiers;
        let key = k.key.as_str();
        let handled = match key {
            "tab" if m.control && !m.platform && !m.alt => {
                self.step_tab(if m.shift { -1 } else { 1 }, cx);
                true
            }
            "e" if m.platform && m.shift && !m.control && !m.alt => {
                self.toggle_tree(cx);
                true
            }
            // ⇧⌘P: this folder's file finder (⌘P stays the app's palette).
            "p" if m.platform && m.shift && !m.control && !m.alt => {
                self.open_finder(window, cx);
                true
            }
            "f" if m.platform && !m.shift && !m.control && !m.alt => {
                self.open_find(window, cx);
                true
            }
            "l" if m.platform && !m.shift && !m.control && !m.alt => {
                self.open_goto(window, cx);
                true
            }
            "g" if m.platform && !m.control && !m.alt && self.find.is_some() => {
                self.find_step(if m.shift { -1 } else { 1 }, cx);
                true
            }
            "escape" if self.finder.is_some() || self.find.is_some() || self.goto.is_some() => {
                self.close_bars(window, cx);
                true
            }
            "s" if m.platform && !m.shift && !m.control && !m.alt => {
                self.save_active(cx);
                true
            }
            // Editing: movement, deletes, Return, Tab, undo and cut (characters come through the input method).
            _ if self.focus.is_focused(window) && self.editing() => self.edit_key(e, cx),
            "escape" if self.selection.is_some() => {
                self.selection = None;
                true
            }
            _ if m.control || m.alt || m.function => false,
            _ if self.tree_focus.is_focused(window) && !m.platform => self.tree_key(key, window, cx),
            _ if self.focus.is_focused(window) => self.text_key(key, m.platform),
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }
}

fn hint_text(text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(text.into())
}

impl Render for FilesPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let browsing = self.browsing();
        let main = if browsing {
            self.render_browser(window, cx)
        } else {
            let bars = self.bars(cx);
            let viewer = self.render_viewer(window, cx);
            div().size_full().flex().flex_col().children(bars).child(div().flex_1().min_h_0().child(viewer)).into_any_element()
        };
        let tree = (self.state.tree && !browsing).then(|| self.render_tree(window, cx));
        let finder = self.render_finder(cx);
        let overlays: Vec<AnyElement> = [self.render_transfer(cx), self.render_preview(cx), self.render_menu(cx)].into_iter().flatten().collect();
        let body = self.body_bounds.clone();
        let pane = cx.entity().downgrade();
        let dragging_tree = matches!(self.drag, Some(Drag::Tree { .. }));
        // While the tree's edge is dragged, moves and the release anywhere.
        let probe = gpui::canvas(
            move |b, _, _| b,
            move |b, _, window, _| {
                body.set(Some(b));
                if dragging_tree {
                    let moves = pane.clone();
                    window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                        if phase == gpui::DispatchPhase::Bubble {
                            let _ = moves.update(cx, |this, cx| this.text_move(e, window, cx));
                        }
                    });
                    let ups = pane.clone();
                    window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Bubble && e.button == MouseButton::Left {
                            let _ = ups.update(cx, |this, cx| this.text_up(cx));
                        }
                    });
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id("files-pane")
            .key_context("FilesPane")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .text_size(px(13.0))
            .text_color(rgb(theme::foreground()))
            .when(dragging_tree, |el| el.cursor_col_resize())
            .on_action(cx.listener(|this, _: &act::ClosePane, window, cx| {
                if this.finder.is_some() || this.find.is_some() || this.goto.is_some() {
                    return this.close_bars(window, cx);
                }
                if this.browsing() {
                    // The browser has no tabs: ⌘W closes the block.
                    return cx.propagate();
                }
                match this.state.active.clone() {
                    Some(path) => this.close(&path, cx),
                    // No tab left: ⌘W closes the block.
                    None => cx.propagate(),
                }
            }))
            .on_action(cx.listener(|this, _: &act::Copy, window, cx| {
                if this.browsing() && this.browser.focus.is_focused(window) {
                    return this.copy_selected(cx);
                }
                match this.selected_text() {
                    Some(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
                    None => cx.propagate(),
                }
            }))
            .on_action(cx.listener(|this, _: &act::SelectAll, window, cx| {
                if this.browsing() {
                    if this.browser.focus.is_focused(window) {
                        this.browse_select_all(cx);
                    } else {
                        cx.propagate();
                    }
                    return;
                }
                this.select_all(cx);
            }))
            .on_action(cx.listener(|this, _: &act::Paste, window, cx| {
                if this.browsing() && this.browser.focus.is_focused(window) {
                    return this.paste_files(cx);
                }
                if !this.paste(cx) {
                    cx.propagate();
                }
            }))
            // ⌘↑ ⌘↓ are the terminal's prompt jumps; editing, they go to the text's start and end; browsing, to
            // the enclosing folder and into the selection.
            .on_action(cx.listener(|this, _: &act::JumpToPreviousPrompt, window, cx| {
                if this.browsing() && this.browser.focus.contains_focused(window, cx) {
                    return this.go_parent(cx);
                }
                if !this.caret_to_end(false, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &act::JumpToNextPrompt, window, cx| {
                if this.browsing() && this.browser.focus.is_focused(window) {
                    return this.open_selected(window, cx);
                }
                if !this.caret_to_end(true, cx) {
                    cx.propagate();
                }
            }))
            // Browsing, Finder's keys win over the app's: ⌘D duplicates, ⌘[ ⌘] go back and forward, ⇧⌘N makes a folder.
            .on_action(cx.listener(|this, _: &act::SplitRight, window, cx| {
                if this.browsing() && this.browser.focus.is_focused(window) {
                    return this.duplicate_selected(cx);
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &act::FocusPreviousPane, window, cx| {
                if this.browsing() && this.browser.focus.contains_focused(window, cx) {
                    return this.go_back(cx);
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &act::FocusNextPane, window, cx| {
                if this.browsing() && this.browser.focus.contains_focused(window, cx) {
                    return this.go_forward(cx);
                }
                cx.propagate();
            }))
            .on_action(cx.listener(|this, _: &act::NewSession, window, cx| {
                if this.browsing() && this.browser.focus.contains_focused(window, cx) {
                    return this.new_folder(window, cx);
                }
                cx.propagate();
            }))
            .on_key_down(cx.listener(Self::key_down))
            // A press anywhere else in the block (a picture, rendered markdown) focuses it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.focus.contains_focused(window, cx) {
                        let focus = if this.browsing() { this.browser.focus.clone() } else { this.focus.clone() };
                        window.focus(&focus, cx);
                    }
                }),
            )
            .child(div().flex_1().min_w_0().h_full().child(main))
            .children(tree)
            .children(finder)
            .children(overlays)
            .child(probe)
    }
}

/// `path` with the home folder as `~` (this Mac's: a remote root shows as is).
fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_owned(),
    }
}
