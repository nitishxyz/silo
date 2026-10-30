//! Pictures and PDFs in the viewer: a PDF's pages drawn as bitmaps sized to
//! the zoom and display (the ones in view, off the UI thread), and zoom for
//! every kind: pictures and PDFs scale around the pointer (pinch, ⌘+ ⌘- ⌘0),
//! text changes its font size.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{AnyElement, Context, PinchEvent, RenderImage, ScrollHandle, Window, div, img, prelude::*, px, rgb};

use super::{Content, FilesPane, model, pdf, zoom};
use crate::theme;

/// An open PDF.
pub(crate) struct PdfFile {
    bytes: Arc<[u8]>,
    pub size: u64,
    /// Each page's size in points.
    pub pages: Vec<(f32, f32)>,
    /// Drawn pages: the bitmap's width in pixels, and the bitmap.
    bitmaps: HashMap<usize, (u32, Arc<RenderImage>)>,
    rendering: bool,
}

/// A picture's or a PDF's zoom (1 fits the view) and scroll.
pub(crate) struct Zoomed {
    pub zoom: f32,
    pub scroll: ScrollHandle,
}

impl Default for Zoomed {
    fn default() -> Self {
        Self { zoom: 1.0, scroll: ScrollHandle::new() }
    }
}

/// Pages drawn per pass.
const BATCH: usize = 3;

impl FilesPane {
    /// A PDF's bytes came: measure its pages (off the UI thread).
    pub(super) fn open_pdf(&mut self, path: String, bytes: Vec<u8>, size: u64, cx: &mut Context<Self>) {
        let bytes: Arc<[u8]> = bytes.into();
        let task = cx.background_spawn({
            let bytes = bytes.clone();
            async move { pdf::page_sizes(&bytes) }
        });
        cx.spawn(async move |this, cx| {
            let pages = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.state.open.contains(&path) {
                    return;
                }
                let content = match pages {
                    Some(pages) => Content::Pdf(Box::new(PdfFile { bytes, size, pages, bitmaps: HashMap::new(), rendering: false })),
                    None => Content::Failed("Not a PDF this Mac can show".into()),
                };
                if let Some(Content::Pdf(old)) = this.contents.insert(path, content) {
                    this.retired.extend(old.bitmaps.into_values().map(|(_, image)| image));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Draw `wanted` pages (index, width) of `path`'s PDF that aren't drawn
    /// at that width yet, a few at a time; pages far from `keep` are let go.
    fn draw_pages(&mut self, path: &str, wanted: Vec<(usize, u32)>, keep: Range<usize>, cx: &mut Context<Self>) {
        let Some(Content::Pdf(file)) = self.contents.get_mut(path) else { return };
        let far: Vec<usize> = file.bitmaps.keys().copied().filter(|i| !keep.contains(i)).collect();
        for i in far {
            if let Some((_, image)) = file.bitmaps.remove(&i) {
                self.retired.push(image);
            }
        }
        let todo: Vec<(usize, u32)> = wanted.into_iter().filter(|(i, w)| file.bitmaps.get(i).is_none_or(|b| b.0 != *w)).take(BATCH).collect();
        if todo.is_empty() || file.rendering {
            return;
        }
        file.rendering = true;
        let bytes = file.bytes.clone();
        let task = cx.background_spawn(async move { pdf::render(&bytes, &todo) });
        let path = path.to_owned();
        cx.spawn(async move |this, cx| {
            let drawn = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(Content::Pdf(file)) = this.contents.get_mut(&path) else { return };
                file.rendering = false;
                for (i, w, image) in drawn {
                    if let Some((_, old)) = file.bitmaps.insert(i, (w, image)) {
                        this.retired.push(old);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Free the bitmaps no longer drawn.
    pub(super) fn drop_retired(&mut self, window: &mut Window) {
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
    }
}

/// The view's size before its first paint.
const VIEW: (f32, f32) = (600.0, 400.0);

impl FilesPane {
    /// The picture or PDF view's size, as last painted.
    fn view_size(&self) -> (f32, f32) {
        self.preview_bounds.get().map_or(VIEW, |b| (f32::from(b.size.width), f32::from(b.size.height)))
    }

    /// The content's size for `path` at `zoom` in a view of `view` (a
    /// picture's without its pixel size doesn't zoom).
    fn content_size(&self, path: &str, zoom: f32, view: (f32, f32)) -> Option<(f32, f32)> {
        match self.contents.get(path)? {
            Content::Pdf(file) => {
                let pages = zoom::pages(&file.pages, view.0, zoom);
                Some((pages.width, pages.height))
            }
            #[allow(clippy::cast_precision_loss)]
            Content::Image { dims: Some((w, h)), .. } => {
                let (w, h) = (*w as f32, *h as f32);
                let fit = zoom::fit(w, h, view.0 - 2.0 * zoom::PAD, view.1 - 2.0 * zoom::PAD);
                Some((w * fit * zoom + 2.0 * zoom::PAD, h * fit * zoom + 2.0 * zoom::PAD))
            }
            _ => None,
        }
    }

    /// Zoom the shown picture or PDF to `to`, keeping the content under
    /// `at` (from the view's top left) in place.
    fn zoom_to(&mut self, path: &str, to: f32, at: (f32, f32), cx: &mut Context<Self>) {
        let view = self.view_size();
        let zoomed = self.zoomed.entry(path.to_owned()).or_default();
        let from = zoomed.zoom;
        let scroll = zoomed.scroll.clone();
        let (Some(before), Some(after)) = (self.content_size(path, from, view), self.content_size(path, to, view)) else { return };
        let offset = scroll.offset();
        let x = zoom::anchored(-f32::from(offset.x), at.0, view.0, before.0, after.0 / before.0);
        let y = zoom::anchored(-f32::from(offset.y), at.1, view.1, before.1, after.1 / before.1);
        scroll.set_offset(gpui::point(px(-x), px(-y)));
        if let Some(zoomed) = self.zoomed.get_mut(path) {
            zoomed.zoom = to;
        }
        cx.notify();
    }

    /// A trackpad pinch over the viewer.
    pub(super) fn pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        if matches!(self.contents.get(&path), Some(Content::Text(_))) {
            return self.set_text_zoom(zoom::pinched(self.text_zoom, event.delta, zoom::TEXT_MIN, zoom::TEXT_MAX), cx);
        }
        let Some(bounds) = self.preview_bounds.get() else { return };
        let at = event.position - bounds.origin;
        let from = self.zoomed.get(&path).map_or(1.0, |z| z.zoom);
        self.zoom_to(&path, zoom::pinched(from, event.delta, zoom::MIN, zoom::MAX), (f32::from(at.x), f32::from(at.y)), cx);
    }

    /// ⌘+ (`dir` 1), ⌘- (-1), ⌘0 (0): whether the shown tab zooms.
    pub(super) fn zoom_key(&mut self, dir: i32, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.state.active.clone() else { return false };
        match self.contents.get(&path) {
            Some(Content::Text(_)) => self.set_text_zoom(zoom::stepped(self.text_zoom, dir, zoom::TEXT_MIN, zoom::TEXT_MAX), cx),
            Some(Content::Pdf(_) | Content::Image { dims: Some(_), .. }) => {
                let from = self.zoomed.get(&path).map_or(1.0, |z| z.zoom);
                let (w, h) = self.view_size();
                self.zoom_to(&path, zoom::stepped(from, dir, zoom::MIN, zoom::MAX), (w / 2.0, h / 2.0), cx);
            }
            _ => return false,
        }
        true
    }

    /// The text's font-size factor, keeping the shown tab's top line.
    fn set_text_zoom(&mut self, to: f32, cx: &mut Context<Self>) {
        if let Some(path) = self.state.active.clone()
            && let Some(top) = self.top_line(&path)
        {
            self.keep_top = Some((path, top));
        }
        self.text_zoom = to;
        cx.notify();
    }
}

impl FilesPane {
    /// A PDF's pages down the view, the ones in view drawn sharp at the
    /// current zoom (the old bitmap scaled meanwhile).
    pub(super) fn render_pdf(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let view = self.view_size();
        let zoomed = self.zoomed.entry(path.to_owned()).or_default();
        let (z, scroll) = (zoomed.zoom, zoomed.scroll.clone());
        let Some(Content::Pdf(file)) = self.contents.get(path) else { return div().into_any_element() };
        let layout = zoom::pages(&file.pages, view.0, z);
        let top = -f32::from(scroll.offset().y);
        let shown = layout.visible(&file.pages, top - view.1 * 0.5, top + view.1 * 1.5);
        let sf = window.scale_factor();
        let wanted: Vec<(usize, u32)> = shown.clone().map(|i| (i, zoom::bitmap_width(file.pages[i].0, file.pages[i].1, layout.scale, sf))).collect();
        let width = layout.width.max(view.0);
        let pages: Vec<AnyElement> = shown
            .clone()
            .map(|i| {
                let (w, h) = (file.pages[i].0 * layout.scale, file.pages[i].1 * layout.scale);
                div()
                    .absolute()
                    .top(px(layout.tops[i]))
                    .left(px((width - w) / 2.0))
                    .w(px(w))
                    .h(px(h))
                    .bg(rgb(0x00ff_ffff))
                    .shadow_sm()
                    .children(file.bitmaps.get(&i).map(|(_, image)| img(image.clone()).size_full()))
                    .into_any_element()
            })
            .collect();
        let count = file.pages.len();
        let caption = format!("PDF · {count} {} · {}", if count == 1 { "page" } else { "pages" }, model::human_size(file.size));
        let keep = shown.start.saturating_sub(2)..shown.end + 2;
        self.draw_pages(path, wanted, keep, cx);
        let content = div().relative().flex_none().w(px(width)).h(px(layout.height)).children(pages);
        self.zoomable(content.into_any_element(), caption, z, &scroll, cx)
    }

    /// A picture with its pixel size: fitted (never upscaled) at zoom 1.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn render_picture(&mut self, path: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Content::Image { image, mime, size, dims: Some((w, h)) }) = self.contents.get(path) else { return None };
        let (image, (w, h)) = (image.clone(), (*w as f32, *h as f32));
        let kind = mime.trim_start_matches("image/").trim_end_matches("+xml").to_uppercase();
        let caption = format!("{kind} · {w}×{h} · {}", model::human_size(*size));
        let view = self.view_size();
        let zoomed = self.zoomed.entry(path.to_owned()).or_default();
        let (z, scroll) = (zoomed.zoom, zoomed.scroll.clone());
        let fit = zoom::fit(w, h, view.0 - 2.0 * zoom::PAD, view.1 - 2.0 * zoom::PAD);
        let (dw, dh) = (w * fit * z, h * fit * z);
        let content = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(px((dw + 2.0 * zoom::PAD).max(view.0)))
            .h(px((dh + 2.0 * zoom::PAD).max(view.1)))
            .child(img(image).flex_none().w(px(dw)).h(px(dh)));
        Some(self.zoomable(content.into_any_element(), caption, z, &scroll, cx))
    }

    /// `content` scrolling both ways under pinches, with `caption` (and the
    /// zoom, when not 1) under it.
    fn zoomable(&self, content: AnyElement, caption: String, z: f32, scroll: &ScrollHandle, cx: &mut Context<Self>) -> AnyElement {
        let bounds = self.preview_bounds.clone();
        let probe = gpui::canvas(move |b, _, _| bounds.set(Some(b)), |_, (), _, _| {}).absolute().top_0().left_0().size_full();
        #[allow(clippy::cast_possible_truncation)]
        let caption = if (z - 1.0).abs() > 0.005 { format!("{caption} · {}%", (z * 100.0).round() as i32) } else { caption };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div().flex_1().min_h_0().relative().child(probe).child(
                    div()
                        .id("files-zoomable")
                        .size_full()
                        .overflow_scroll()
                        .track_scroll(scroll)
                        .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| this.pinch(e, cx)))
                        .child(content),
                ),
            )
            .child(div().flex_none().flex().justify_center().py(px(6.0)).text_color(rgb(theme::muted())).child(caption))
            .into_any_element()
    }
}

impl FilesPane {
    /// Let go of `content`, its bitmaps freed at the next render.
    pub(super) fn retire(&mut self, content: Content) {
        if let Content::Pdf(file) = content {
            self.retired.extend(file.bitmaps.into_values().map(|(_, image)| image));
        }
    }
}
