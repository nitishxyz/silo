//! The viewer's image chrome (toolbar, sides, footer), an SVG's sharp detail past gpui's raster, and Copy and
//! Save from the loaded media.

use std::{sync::Arc, time::Instant};

use gpui::Context;

use super::{DIM, Geo, INK, Item, MediaViewer, NOTE, ViewerEvent, geo::BOTTOM, geo::TOP};
use crate::agent::{
    av,
    media::{Loaded, Media},
    svg,
    view::tabular,
};

/// A raster of the part of an SVG that shows at the zoomed scale (`svg.rs`).
pub(super) struct Detail {
    /// The media key, the scale (px a unit, ×1000) and the region in pixels it was made for.
    key: (String, [i64; 5]),
    region: svg::Rect,
    pub image: Arc<gpui::RenderImage>,
}

/// `1024×1024 · PNG · 1.2 MB · OpenAI gpt-image · 2 of 5`: what is known of the item (`dims` decoded fill in
/// its own), its credit, and where it is among `count`.
#[must_use]
pub fn caption(item: &Item, count: usize, ix: usize, dims: Option<(f32, f32)>) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let decoded = dims.map(|(w, h)| (w as u32, h as u32));
    let media = Media {
        id: item.id.clone(),
        name: item.name.clone(),
        mime: item.mime.clone(),
        size: item.size.clone().unwrap_or_default(),
        dims: item.dims.or(decoded),
        ms: item.ms,
    };
    std::iter::once(media.caption())
        .chain(item.credit.clone().filter(|c| !c.trim().is_empty()))
        .chain((count > 1).then(|| format!("{} of {count}", ix + 1)))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The footer's first line: the item's title, else its file's name.
#[must_use]
pub fn title(item: &Item) -> &str {
    item.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&item.name)
}

/// The prompt's line under the caption (all of it on hover), when there is one.
#[must_use]
pub fn prompt(item: &Item) -> Option<&str> {
    item.prompt.as_deref().filter(|p| !p.is_empty())
}

/// A toolbar button: an icon or a short label.
fn tool(id: &'static str, label: Option<&'static str>, glyph: Option<&'static str>) -> gpui::Stateful<gpui::Div> {
    use gpui::{div, prelude::*, px, rgb};
    div()
        .id(id)
        .flex_none()
        .h(px(28.0))
        .min_w(px(28.0))
        .px(px(if label.is_some() { 9.0 } else { 0.0 }))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.0))
        .text_size(px(12.0))
        .text_color(rgb(INK))
        .cursor_pointer()
        .hover(|s| s.bg(gpui::hsla(0.0, 0.0, 1.0, 0.1)))
        .active(|s| s.bg(gpui::hsla(0.0, 0.0, 1.0, 0.16)))
        .children(label)
        .children(glyph.map(|g| crate::assets::icon(g, 14.0, INK)))
}

/// A thin divider between the toolbar's groups.
fn divider() -> gpui::Div {
    use gpui::{div, prelude::*, px};
    div().flex_none().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(gpui::hsla(0.0, 0.0, 1.0, 0.14))
}

/// A frosted dark chip the toolbar and side buttons sit on.
fn chip() -> gpui::Div {
    use gpui::{div, prelude::*, px};
    div()
        .occlude()
        .flex()
        .items_center()
        .p(px(3.0))
        .rounded(px(10.0))
        .bg(gpui::hsla(0.0, 0.0, 0.08, 0.78))
        .border_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
}

impl MediaViewer {
    /// Zoom %, Fit, 1:1, Copy, Save…, Close (a video: a passing note and Close).
    pub(super) fn toolbar(&self, scale: f32, note: Option<&'static str>, ready: bool, video: bool, cx: &mut Context<Self>) -> gpui::Div {
        use gpui::{ClickEvent, div, prelude::*, px, rgb};
        #[allow(clippy::cast_possible_truncation)]
        let pct = format!("{}%", (scale * 100.0).round() as i32);
        // A video has no zoom: only a passing note shows there.
        let label = (!video || note.is_some()).then(|| {
            div()
                .min_w(px(52.0))
                .px(px(6.0))
                .flex()
                .justify_center()
                .text_size(px(12.0))
                .text_color(rgb(if note.is_some() { INK } else { DIM }))
                .font_features(tabular())
                .child(note.map_or(pct, str::to_owned))
        });
        let ready_only = |el: gpui::Stateful<gpui::Div>| if ready { el } else { el.opacity(0.4) };
        let zoom = (!video).then(|| {
            div()
                .flex()
                .items_center()
                .child(
                    tool("viewer-fit", Some("Fit"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Fit  0"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_to(None, None, true, cx))),
                )
                .child(
                    tool("viewer-actual", Some("1:1"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Actual size  1"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_to(Some(1.0), None, true, cx))),
                )
                .child(divider())
        });
        // A video's Copy and Save… sit in its player's bar.
        let files = (!video).then(|| {
            div()
                .flex()
                .items_center()
                .child(ready_only(
                    tool("viewer-copy", None, Some("copy"))
                        .tooltip(crate::tooltip::Tooltip::text("Copy image  ⌘C"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy(cx))),
                ))
                .child(ready_only(
                    tool("viewer-save", Some("Save…"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Save  ⌘S"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx))),
                ))
                .child(divider())
        });
        chip().children(label.map(|l| div().flex().items_center().child(l).child(divider()))).children(zoom).children(files).child(
            tool("viewer-close", None, Some("close"))
                .tooltip(crate::tooltip::Tooltip::text("Close  esc"))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close(window, cx))),
        )
    }

    /// Previous / next at the sides (hidden at the ends).
    pub(super) fn sides(ix: usize, count: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui::{ClickEvent, div, prelude::*, px};
        let side = |id: &'static str, dir: i32, cx: &mut Context<Self>| {
            let glyph = crate::assets::icon("chevron-right", 16.0, INK)
                .when(dir < 0, |g| g.with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::PI))));
            chip().rounded_full().child(
                tool(id, None, None)
                    .size(px(32.0))
                    .rounded_full()
                    .child(glyph)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.step(dir, cx))),
            )
        };
        let middle = |el: gpui::Div| el.absolute().top_0().bottom(px(BOTTOM - TOP)).flex().items_center();
        div()
            .absolute()
            .size_full()
            .when(ix > 0, |el| el.child(middle(div().left(px(12.0))).child(side("viewer-prev", -1, cx))))
            .when(ix + 1 < count, |el| el.child(middle(div().right(px(12.0))).child(side("viewer-next", 1, cx))))
            .into_any_element()
    }
}

/// The [`title`]; [`caption`]; the prompt on one line (all of it on hover).
pub(super) fn footer(item: &Item, count: usize, ix: usize, dims: Option<(f32, f32)>) -> gpui::Div {
    use gpui::{FontWeight, div, prelude::*, px, rgb};
    let caption = caption(item, count, ix, dims);
    let line = |el: gpui::Div| el.max_w_full().overflow_hidden().whitespace_nowrap().text_ellipsis();
    let prompt = prompt(item).map(|p| {
        div()
            .id("viewer-prompt")
            .max_w_full()
            .child(line(div()).text_color(rgb(DIM)).child(crate::agent::transcript::clip(p, 400)))
            .tooltip(crate::tooltip::Tooltip::text(p.to_owned()))
    });
    div().flex().justify_center().child(
        div()
            .occlude()
            .max_w(px(640.0))
            .min_w_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(2.0))
            .text_size(px(12.0))
            .child(line(div()).text_size(px(13.0)).font_weight(FontWeight::MEDIUM).text_color(rgb(INK)).child(title(item).to_owned()))
            .child(line(div()).text_color(rgb(DIM)).font_features(tabular()).child(caption))
            .children(prompt),
    )
}

impl MediaViewer {
    /// A word in the toolbar for a moment (the host hears it too).
    fn note(&mut self, note: &'static str, cx: &mut Context<Self>) {
        self.note = Some((note, Instant::now()));
        cx.emit(ViewerEvent::Note(note));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// An image to the clipboard; a video as its file (named as made, next to the cache file).
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some((item, c)) = self.viewed_clip(cx) {
            let named = c.file.with_extension("").join(item.name.replace(['/', ':'], "-"));
            let linked =
                named.exists() || (named.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::hard_link(&c.file, &named).is_ok());
            return self.note(if linked && av::copy_file(&named) { "Copied" } else { "Couldn't copy" }, cx);
        }
        let Some((item, l)) = self.viewed(cx) else { return };
        // An SVG goes as its file (a raster would lose it); anything else as the image.
        if l.svg.is_some() {
            let copied = svg_file(&item, &l).is_some_and(|f| av::copy_file(&f));
            return self.note(if copied { "Copied" } else { "Couldn't copy" }, cx);
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&l.image));
        self.note("Copied", cx);
    }

    /// The save panel (Downloads, the file's name), then its bytes written there.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        if let Some((item, c)) = self.viewed_clip(cx) {
            let saved = save_copy(&item.name, c.file.clone(), cx);
            return cx
                .spawn(async move |this, cx| {
                    let Some(wrote) = saved.await else { return };
                    let _ = this.update(cx, |this, cx| this.note(if wrote { "Saved" } else { "Couldn't save" }, cx));
                })
                .detach();
        }
        let Some((item, l)) = self.viewed(cx) else { return };
        let chosen = cx.prompt_for_new_path(&silo_web::downloads_dir(), Some(&item.name));
        cx.spawn(async move |this, cx| {
            let Some(path) = chosen.await.ok().and_then(Result::ok).flatten() else { return };
            let wrote = cx.background_executor().spawn(async move { std::fs::write(&path, l.image.bytes()) }).await;
            let _ = this.update(cx, |this, cx| this.note(if wrote.is_ok() { "Saved" } else { "Couldn't save" }, cx));
        })
        .detach();
    }

    /// An SVG drawn past gpui's raster (2 px a unit): a raster of just what shows at `scale` (and `device` px a
    /// point), made off the main thread once the view settles; one at a time, the newest view next. Its region
    /// (image units) and image while one fits this SVG (media `key`), placed by the scale drawn now.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn svg_detail(
        &mut self,
        key: &str,
        l: &Loaded,
        g: &Geo,
        (scale, pan): (f32, (f32, f32)),
        device: f32,
        settled: bool,
        cx: &mut Context<Self>,
    ) -> Option<(svg::Rect, Arc<gpui::RenderImage>)> {
        let doc = l.svg.clone()?;
        let ds = scale * device;
        if ds <= 2.05 {
            self.retired.extend(self.detail.take().map(|d| d.image));
            return None;
        }
        let (w, h) = (g.dims.0 * scale, g.dims.1 * scale);
        let origin = (g.center.0 + pan.0 - w / 2.0, g.center.1 + pan.1 - h / 2.0);
        let region = svg::visible(g.dims, origin, scale, g.view, device)?;
        #[allow(clippy::cast_possible_truncation)]
        let px = |f: f32| f64::from(f).round() as i64;
        let want = (key.to_owned(), [px(ds * 1000.0), px(region.0 * ds), px(region.1 * ds), px(region.2 * ds), px(region.3 * ds)]);
        if settled && !self.detail_pending && self.detail.as_ref().is_none_or(|d| d.key != want) {
            self.detail_pending = true;
            let dims = g.dims;
            let made = cx.background_executor().spawn(async move { doc.render(dims, region, ds) });
            cx.spawn(async move |this, cx| {
                let image = made.await;
                let _ = this.update(cx, |this, cx| {
                    this.detail_pending = false;
                    if let Some(image) = image.filter(|_| this.key.as_deref() == Some(want.0.as_str())) {
                        let old = this.detail.replace(Detail { key: want, region, image: Arc::new(image) });
                        this.retired.extend(old.map(|d| d.image));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        self.detail.as_ref().filter(|d| d.key.0 == key).map(|d| (d.region, d.image.clone()))
    }
}

/// The shown SVG as a file in the item's scratch folder (named as made), for the clipboard.
fn svg_file(item: &Item, l: &Loaded) -> Option<std::path::PathBuf> {
    let file = item.scratch.as_ref()?.join(item.name.replace(['/', ':'], "-"));
    if !file.exists() {
        std::fs::create_dir_all(file.parent()?).ok()?;
        std::fs::write(&file, l.image.bytes()).ok()?;
    }
    Some(file)
}

/// The save panel (Downloads, `name`), then `file` copied there; `None` when cancelled.
fn save_copy<V: 'static>(name: &str, file: std::path::PathBuf, cx: &mut Context<V>) -> gpui::Task<Option<bool>> {
    let chosen = cx.prompt_for_new_path(&silo_web::downloads_dir(), Some(name));
    cx.spawn(async move |_, cx| {
        let path = chosen.await.ok().and_then(Result::ok).flatten()?;
        Some(cx.background_executor().spawn(async move { std::fs::copy(&file, &path).is_ok() }).await)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_viewer::Kind;

    fn item() -> Item {
        Item {
            id: "att_1".into(),
            key: "att_1".into(),
            name: "dunes.png".into(),
            title: None,
            kind: Kind::Image,
            mime: "image/png".into(),
            size: Some("1.2 MB".into()),
            dims: Some((1024, 1024)),
            ms: None,
            credit: Some("OpenAI gpt-image".into()),
            prompt: Some("Dunes at dusk".into()),
            scratch: None,
        }
    }

    #[test]
    fn the_footer_says_what_is_known() {
        assert_eq!(caption(&item(), 5, 1, None), "1024×1024 · PNG · 1.2 MB · OpenAI gpt-image · 2 of 5");
        let bare = Item { size: None, dims: None, credit: None, prompt: None, ..item() };
        assert_eq!(caption(&bare, 1, 0, None), "PNG", "nothing optional, alone");
        assert_eq!(caption(&bare, 1, 0, Some((640.0, 480.0))), "640×480 · PNG", "decoded dimensions fill in");
        let video = Item { mime: "video/mp4".into(), dims: Some((1280, 720)), ms: Some(8000), size: Some("4.2 MB".into()), ..bare.clone() };
        assert_eq!(caption(&video, 1, 0, None), "1280×720 · MP4 · 0:08 · 4.2 MB");
        assert_eq!(caption(&Item { mime: String::new(), ..bare.clone() }, 2, 1, None), "2 of 2", "no format: just where");
        assert_eq!(prompt(&item()), Some("Dunes at dusk"));
        assert_eq!(prompt(&Item { prompt: Some(String::new()), ..item() }), None);
        assert_eq!(prompt(&bare), None);
        assert_eq!((title(&bare), title(&Item { title: Some("Mara".into()), ..bare.clone() })), ("dunes.png", "Mara"));
    }
}
