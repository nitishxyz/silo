//! Files for a prompt (`docs/AGENT_BLOCK.md` §4c): read from a drop, paste or
//! the picker, uploaded with `attachment.put` when sent, shown as chips in
//! the composer and on the user's message.

use std::{sync::Arc, time::Instant};

use gpui::{AnyElement, Context, Div, ObjectFit, div, img, prelude::*, px, relative, rgb};
use otto_client::{ClientError, Connection};
use otto_llm::{Media, MediaSource, Part};
use otto_proto::InputPart;

use super::{AgentPane, transcript::FileChip, view::ink};
use crate::{
    assets::icon,
    theme::{self, Continuous as _},
};

/// A file for the next prompt, uploaded to otto when it is sent (`attachment.put`).
#[derive(Clone)]
pub(super) struct Attachment {
    pub(super) name: String,
    pub(super) mime: String,
    pub(super) bytes: Arc<Vec<u8>>,
    /// Images: the chip's thumbnail.
    pub(super) thumb: Option<Arc<gpui::Image>>,
    /// Added at (the chip fades in).
    pub(super) at: Instant,
}

impl Attachment {
    /// Read in the background; the error names the file, never its path.
    pub(super) fn read(path: &std::path::Path) -> Result<Self, String> {
        let name = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        match std::fs::metadata(path) {
            Ok(m) if m.len() > otto_proto::MAX_ATTACHMENT as u64 => return Err(format!("{name} is over 25 MB")),
            Err(_) => return Err(format!("Couldn't read {name}")),
            _ => {}
        }
        let bytes = std::fs::read(path).map_err(|_| format!("Couldn't read {name}"))?;
        Self::new(name, bytes)
    }

    pub(super) fn new(name: String, bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() > otto_proto::MAX_ATTACHMENT {
            return Err(format!("{name} is over 25 MB"));
        }
        let mime = mime_for(&name, &bytes).to_owned();
        let thumb = mime.starts_with("image/").then(|| thumbnail(&bytes)).flatten();
        Ok(Self { name, mime, bytes: Arc::new(bytes), thumb, at: Instant::now() })
    }
}

/// A file's mime type from its first bytes, else its extension.
fn mime_for(name: &str, bytes: &[u8]) -> &'static str {
    if let Ok(format) = image::guess_format(bytes) {
        return format.to_mime_type();
    }
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf";
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "heic" => "image/heic",
        "mov" => "video/quicktime",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        // Valid UTF-8, or cut inside a character at the end.
        _ if !bytes.contains(&0) && std::str::from_utf8(&bytes[..bytes.len().min(4096)]).map_or_else(|e| e.error_len().is_none(), |_| true) => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
}

/// A small PNG of an image for a chip.
fn thumbnail(bytes: &[u8]) -> Option<Arc<gpui::Image>> {
    let image = image::load_from_memory(bytes).ok()?.thumbnail(96, 96);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png.into_inner())))
}

/// Upload `files` to the session in chunks under the frame limit; the
/// prompt's input names them, then the text (and its mentions).
pub(super) async fn upload(c: &Connection, session: &str, files: Vec<Attachment>, text: Vec<InputPart>) -> Result<Vec<InputPart>, ClientError> {
    let mut input = Vec::new();
    for file in files {
        let chunks: Vec<&[u8]> = if file.bytes.is_empty() { vec![&[]] } else { file.bytes.chunks(otto_proto::ATTACHMENT_CHUNK).collect() };
        let mut id: Option<String> = None;
        for chunk in chunks {
            let stored = c.attachment_put(session, &file.name, &file.mime, data_encoding::BASE64.encode(chunk), id.clone()).await?;
            id = Some(stored.id);
        }
        input.extend(id.map(|id| InputPart::Attachment { id }));
    }
    input.extend(text);
    Ok(input)
}

/// The chip's glyph: an image's thumbnail, else its kind's icon.
pub(super) fn glyph(thumb: Option<Arc<gpui::Image>>, mime: &str) -> AnyElement {
    match thumb {
        Some(thumb) => img(thumb).flex_none().size(px(16.0)).rounded(px(3.0)).object_fit(ObjectFit::Cover).into_any_element(),
        None => icon(if mime.starts_with("image/") { "image" } else { "file-text" }, 12.0, theme::muted()).into_any_element(),
    }
}

/// A file's chip: glyph and name, never a path (the composer adds its ×).
pub(super) fn chip(thumb: Option<Arc<gpui::Image>>, mime: &str, name: String) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(24.0))
        .pl(px(if thumb.is_some() { 4.0 } else { 8.0 }))
        .pr(px(8.0))
        .capsule()
        .bg(rgb(ink(0.08)))
        .text_size(px(12.0))
        .text_color(rgb(theme::muted()))
        .child(glyph(thumb, mime))
        .child(div().max_w(px(160.0)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(name))
}

impl AgentPane {
    /// The user's message: its files' chips, then the bubble (its mentions as chips inline).
    pub(super) fn render_user(&mut self, text: &str, files: &[FileChip], mentions: &[super::mention::Chip], cx: &mut Context<Self>) -> AnyElement {
        let chips: Vec<Div> = files
            .iter()
            .map(|f| {
                let thumb = f.image.as_deref().and_then(|key| self.thumb(key, cx));
                chip(thumb, &f.mime, f.name.clone())
            })
            .collect();
        let bubble = (!text.is_empty()).then(|| {
            div()
                .max_w(relative(0.8))
                .px(px(12.0))
                .py(px(8.0))
                .continuous(14.0)
                .bg(rgb(ink(0.06)))
                .text_color(rgb(ink(0.85)))
                .line_height(px(21.0))
                .map(|el| {
                    // Selectable as the row's one leaf; mentions show as chips inline.
                    let (text_el, layout) = super::mention::inline(text, mentions, super::select::selected(super::select::USER));
                    el.child(super::select::register(super::select::USER, 0, None, &layout, text_el))
                })
        });
        div()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(6.0))
            .when(!chips.is_empty(), |el| el.child(div().max_w(relative(0.8)).flex().flex_wrap().justify_end().gap(px(6.0)).children(chips)))
            .children(bubble)
            .into_any_element()
    }

    /// The thumbnail of transcript image `key` (`item#part`), made in the background on first use.
    fn thumb(&mut self, key: &str, cx: &mut Context<Self>) -> Option<Arc<gpui::Image>> {
        if let Some(thumb) = self.thumbs.get(key) {
            return thumb.clone();
        }
        self.thumbs.insert(key.to_owned(), None);
        let (item, part) = key.rsplit_once('#')?;
        let part: usize = part.parse().ok()?;
        let data = match self.mirror()?.items.iter().find(|i| i.id == item)?.parts.get(part)? {
            Part::Image(Media { source: MediaSource::Base64 { data }, .. }) => data.clone(),
            _ => return None,
        };
        let key = key.to_owned();
        let made = cx.background_executor().spawn(async move { data_encoding::BASE64.decode(data.as_bytes()).ok().and_then(|b| thumbnail(&b)) });
        cx.spawn(async move |this, cx| {
            let thumb = made.await;
            let _ = this.update(cx, |this, cx| {
                this.thumbs.insert(key, thumb);
                cx.notify();
            });
        })
        .detach();
        None
    }
}
