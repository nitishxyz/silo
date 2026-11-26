//! A canvas's documents (docs/STUDIO.md §4.2): the text files the agent writes next to it
//! (`canvases/<name>/script.md`, `references.md`). The Documents button at the board's top-left lists them
//! (`script.md` first, a mark on those pinned as document cards); the document panel on the right shows one
//! rendered, with Edit (the World view's inline editor and save), Open in Files and Add to canvas (or Show on
//! canvas when pinned). It follows the file: the store reads it again when the server says the canvas's folder
//! changed. The panel opens from the list, from a document card (double-click, Enter, Open) and from outside
//! ([`StudioPane::open_document`]).

use gpui::{AnyElement, Context, Entity, FontWeight, div, prelude::*, px, rgb};
use silo_proto::studio::Canvas;

use super::{EditTarget, StudioPane, StudioPaneEvent, canvas::CanvasView, centered, store, viewer::Layer, world::pill};
use crate::{
    assets::icon,
    surface::Glassy as _,
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

/// The document panel's width.
const PANEL_W: f32 = 420.0;

/// A row of the Documents list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocRow {
    /// Root-relative.
    pub path: String,
    pub name: String,
    /// The document card showing it, when pinned.
    pub node: Option<String>,
}

/// The Documents list: `docs` (listed in order) with the node each is pinned as on `canvas`.
#[must_use]
pub(crate) fn rows(docs: &[String], canvas: Option<&Canvas>) -> Vec<DocRow> {
    docs.iter()
        .map(|path| DocRow {
            path: path.clone(),
            name: file_name(path).to_owned(),
            node: canvas.and_then(|c| c.nodes.iter().find(|n| c.document_of(&n.id) == Some(path.as_str())).map(|n| n.id.clone())),
        })
        .collect()
}

#[must_use]
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The canvas a document belongs to: `canvases/<name>/<file>` with `file` a document.
#[must_use]
pub(crate) fn canvas_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("canvases/")?;
    let (name, file) = rest.split_once('/')?;
    (!name.is_empty() && silo_proto::studio::is_document(file)).then_some(name)
}

impl StudioPane {
    /// Show document `path` (root-relative) in the panel, its canvas shown.
    pub(crate) fn open_document(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(canvas) = canvas_of(&path)
            && self.state.active.as_deref() != Some(canvas)
        {
            self.show_canvas(canvas.to_owned(), cx);
        }
        self.docs_open = false;
        self.document = Some(path);
        cx.notify();
    }

    pub(crate) fn close_document(&mut self, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|e| self.document.as_ref().is_some_and(|d| e.target == EditTarget::File(d.clone()))) {
            self.editing = None;
        }
        self.document = None;
        cx.notify();
    }

    /// The canvas `name` with its Documents button and, when open, the list and the panel over it.
    pub(super) fn render_canvas(&mut self, name: &str, view: Entity<CanvasView>, cx: &mut Context<Self>) -> AnyElement {
        let layers = view.read(cx).layers(cx);
        let docs = self.store.update(cx, |s, cx| s.documents(name, cx)).unwrap_or_default();
        let canvas = self.store.read(cx).canvas(name).cloned();
        let rows = rows(&docs, canvas.as_deref());
        let button = layers.contains(&Layer::Documents).then(|| self.documents_button(rows.len(), cx));
        let list = (self.docs_open && button.is_some()).then(|| self.documents_list(name, &rows, cx));
        let shown = self.document.clone().filter(|d| canvas_of(d) == Some(name) && layers.contains(&Layer::DocumentPanel));
        let panel = shown.map(|path| {
            let node =
                rows.iter().find(|r| r.path == path).and_then(|r| r.node.clone()).or_else(|| {
                    canvas.as_ref().and_then(|c| c.nodes.iter().find(|n| c.document_of(&n.id) == Some(path.as_str())).map(|n| n.id.clone()))
                });
            self.document_panel(path, node, &view, cx)
        });
        let open = self.document.is_some() || self.docs_open;
        div()
            .relative()
            .size_full()
            .when(open, |el| {
                // Esc closes the list, then the panel, when nothing on the board would take it first.
                el.capture_key_down(cx.listener(move |this, e: &gpui::KeyDownEvent, _, cx| {
                    if e.keystroke.key != "escape" || this.editing.is_some() || this.canvas_busy(cx) {
                        return;
                    }
                    if this.docs_open {
                        this.docs_open = false;
                        cx.notify();
                    } else {
                        this.close_document(cx);
                    }
                    cx.stop_propagation();
                }))
            })
            .child(view)
            .children(button.map(|b| div().absolute().top(px(12.0)).left(px(12.0)).flex().flex_col().gap(px(6.0)).child(b).children(list)))
            .children(panel)
            .into_any_element()
    }

    /// The active canvas has something of its own open that Esc closes (a menu, an edit, the prompt, a picker).
    fn canvas_busy(&self, cx: &gpui::App) -> bool {
        let Some(name) = self.state.active.as_deref() else { return false };
        self.canvas_entity(name).is_some_and(|v| {
            let v = v.read(cx);
            v.menu.is_some() || v.inline.is_some() || v.prompt.is_some() || v.picker.is_some() || v.confirm.is_some() || v.settings.is_some()
        })
    }

    fn documents_button(&self, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let on = self.docs_open;
        div()
            .id("studio-documents")
            .occlude()
            .flex_none()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .glass(crate::radius::CONTROL + 4.0)
            .cursor_pointer()
            .text_size(px(12.5))
            .when(on, |el| el.bg(theme::row_fill()))
            .hover(|s| s.bg(theme::hover_fill()))
            .child(icon("file-text", 13.0, theme::muted()))
            .child("Documents")
            .child(div().text_color(rgb(theme::muted())).child(count.to_string()))
            .tooltip(Tooltip::text("The text files in this canvas's folder"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.docs_open = !this.docs_open;
                cx.notify();
            }))
            .into_any_element()
    }

    fn documents_list(&self, canvas: &str, rows: &[DocRow], cx: &mut Context<Self>) -> AnyElement {
        let items = rows.iter().enumerate().map(|(i, r)| {
            let (path, node, canvas) = (r.path.clone(), r.node.clone(), canvas.to_owned());
            div()
                .id(("studio-document-row", i))
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(28.0))
                .px(px(8.0))
                .continuous(crate::radius::MENU_ROW)
                .cursor_pointer()
                .when(self.document.as_deref() == Some(r.path.as_str()), |el| el.bg(theme::row_fill()))
                .hover(|s| s.bg(theme::hover_fill()))
                .child(icon("file-text", 12.0, theme::muted()))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(r.name.clone()))
                .when(r.node.is_some(), |el| el.child(div().flex_none().text_size(px(11.0)).text_color(rgb(theme::accent())).child("On canvas")))
                .on_click(cx.listener(move |this, _, window, cx| {
                    // Pinned: the card revealed too.
                    if let Some(node) = &node {
                        this.reveal_node(canvas.clone(), node.clone(), window, cx);
                    }
                    this.open_document(path.clone(), cx);
                }))
        });
        let items: Vec<_> = items.collect();
        let empty = items.is_empty();
        div()
            .id("studio-documents-list")
            .occlude()
            .w(px(280.0))
            .max_h(px(360.0))
            .flex()
            .flex_col()
            .p(px(4.0))
            .glass_sheet(crate::radius::CARD)
            .text_size(px(12.5))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.docs_open = false;
                cx.notify();
            }))
            .children(items)
            .when(empty, |el| el.child(div().p(px(8.0)).text_color(rgb(theme::muted())).child(format!("No .md or .txt files in canvases/{canvas}/"))))
            .into_any_element()
    }

    fn document_panel(&mut self, path: String, node: Option<String>, view: &Entity<CanvasView>, cx: &mut Context<Self>) -> AnyElement {
        let target = EditTarget::File(path.clone());
        let editor = self.editor_for(&target, cx);
        let editing = editor.is_some();
        let asset = self.store.update(cx, |s, cx| s.asset(&path, store::AssetKind::Text, cx));
        let text: Option<std::sync::Arc<str>> = match asset {
            store::Asset::Text(t) => Some(t),
            store::Asset::Failed => Some("".into()),
            _ => None,
        };
        let body = match (editor, &text) {
            (Some(e), _) => div().id("studio-document-edit").size_full().overflow_y_scroll().child(e).into_any_element(),
            (None, Some(t)) if t.trim().is_empty() => centered("Nothing written yet".into()),
            (None, Some(t)) => self.markdown(&format!("document:{path}"), t, 16.0),
            (None, None) => centered("Loading\u{2026}".into()),
        };
        let close = div()
            .id("studio-document-close")
            .flex_none()
            .size(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(crate::radius::CONTROL)
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child(icon("close", 11.0, theme::muted()))
            .tooltip(Tooltip::text("Close  esc"))
            .on_click(cx.listener(|this, _, _, cx| this.close_document(cx)));
        let edit = (!editing).then(|| {
            let (path, text) = (path.clone(), text.clone().unwrap_or_else(|| "".into()));
            pill("studio-document-edit-button", "Edit").on_click(cx.listener(move |this, _, window, cx| {
                this.start_edit(EditTarget::File(path.clone()), &text, window, cx);
            }))
        });
        let files = {
            let path = path.clone();
            pill("studio-document-files", "Open in Files").on_click(cx.listener(move |_, _, _, cx| cx.emit(StudioPaneEvent::OpenFile(path.clone()))))
        };
        let place = match node {
            Some(node) => {
                let view = view.clone();
                pill("studio-document-show", "Show on canvas")
                    .on_click(cx.listener(move |_, _, window, cx| view.update(cx, |v, cx| v.reveal_node(&node, window, cx))))
            }
            None => {
                let (view, path) = (view.clone(), path.clone());
                pill("studio-document-add", "Add to canvas")
                    .on_click(cx.listener(move |_, _, window, cx| view.update(cx, |v, cx| v.add_document(&path, window, cx))))
            }
        };
        div()
            .id("studio-document-panel")
            .occlude()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(PANEL_W))
            .flex()
            .flex_col()
            .bg(rgb(theme::background()))
            .border_l_1()
            .border_color(crate::surface::hairline())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon("file-text", 14.0, theme::muted()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(file_name(&path).to_owned()),
                            )
                            .child(close),
                    )
                    .child(div().flex().flex_wrap().gap(px(6.0)).children(edit).child(files).child(place)),
            )
            .child(div().flex_none().h(px(1.0)).bg(crate::surface::hairline()))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::studio::{Canvas, Node, NodeBody, StudioMedia};

    use super::*;

    fn node(id: &str, path: &str, media: StudioMedia) -> Node {
        let body = NodeBody::Asset { path: path.into(), media, width: None, height: None, seconds: None, prompt: None };
        serde_json::from_value(serde_json::json!({"id": id, "x": 0, "y": 0, "w": 320, "h": 400, "body": body})).unwrap()
    }

    #[test]
    fn the_list_marks_what_is_pinned() {
        let docs = silo_proto::studio::documents_of("ep", ["notes.md", "canvas.json", "script.md", "brief.txt"].map(String::from));
        let mut canvas: Canvas = serde_json::from_value(serde_json::json!({"name": "ep", "title": "Ep"})).unwrap();
        canvas.nodes = vec![node("n_img", "canvases/ep/assets/a.png", StudioMedia::Image), node("n_doc", "canvases/ep/script.md", StudioMedia::Text)];
        let got = rows(&docs, Some(&canvas));
        let shown: Vec<(&str, Option<&str>)> = got.iter().map(|r| (r.name.as_str(), r.node.as_deref())).collect();
        assert_eq!(shown, [("script.md", Some("n_doc")), ("brief.txt", None), ("notes.md", None)]);
        assert_eq!(got[0].path, "canvases/ep/script.md");
        assert!(rows(&docs, None).iter().all(|r| r.node.is_none()));
    }

    #[test]
    fn a_documents_canvas_comes_from_its_path() {
        assert_eq!(canvas_of("canvases/intro-theme/script.md"), Some("intro-theme"));
        assert_eq!(canvas_of("canvases/moodboard/references.txt"), Some("moodboard"));
        assert_eq!(canvas_of("canvases/ep/assets/notes.md"), None, "only the folder's own files");
        assert_eq!(canvas_of("canvases/ep/canvas.json"), None);
        assert_eq!(canvas_of("world/overview.md"), None);
    }
}
