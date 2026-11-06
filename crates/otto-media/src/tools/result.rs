//! A generation's result: attachments, project copies, the `[generated …]` lines (§6.3) and the images to look at.
use super::{noun, save};
use crate::{MediaKind, MediaProvider, Output, probe};
use otto_tools::{OutputPart, ToolCx, ToolError, ToolOutput, attachments, images};
use serde_json::{Value, json};

/// The result's lines, metadata entries and images for the model.
type Stored = (Vec<String>, Vec<Value>, Vec<OutputPart>);

/// The long edge of the images the model sees.
const VIEW_EDGE: u32 = 768;

/// A file's size in pixels and length in seconds, where its type has them and they are readable.
pub(crate) fn measure(bytes: &[u8], mime: &str) -> (Option<(u32, u32)>, Option<f64>) {
    match mime {
        "image/svg+xml" => (probe::svg_size(bytes), None),
        m if m.starts_with("image/") => (images::describe(bytes).map(|(d, _)| d), None),
        "video/mp4" | "video/quicktime" => {
            let info = probe::mp4(bytes);
            (info.size, info.seconds)
        }
        "audio/mpeg" => (None, probe::mp3_seconds(bytes)),
        _ => (None, None),
    }
}

/// `8`, `2.5`: whole seconds from 10 up, tenths below.
fn shown_seconds(seconds: f64) -> String {
    let rounded = if seconds >= 10.0 { seconds.round() } else { (seconds * 10.0).round() / 10.0 };
    format!("{rounded}")
}

/// `[generated <id>: <name>, <mime>, <size>[, <w>×<h>][, <seconds> s] — copy_attachment can save it into the project]`
pub(crate) fn line(stored: &attachments::Attachment, size: Option<(u32, u32)>, seconds: Option<f64>) -> String {
    let size_part = size.map_or_else(String::new, |(w, h)| format!(", {w}×{h}"));
    let seconds_part = seconds.map_or_else(String::new, |s| format!(", {} s", shown_seconds(s)));
    format!(
        "[generated {}: {}, {}, {}{size_part}{seconds_part}{}]",
        stored.id,
        stored.name,
        stored.mime,
        attachments::size(stored.size),
        attachments::NOTE_HINT
    )
}

/// What the model sees of an image: it downscaled; a GIF's first frame; nothing of an SVG, a video or audio.
fn view(bytes: &[u8], mime: &str) -> Option<images::Prepared> {
    match mime {
        "image/gif" => {
            let mut first = std::io::Cursor::new(Vec::new());
            image::load_from_memory(bytes).ok()?.write_to(&mut first, image::ImageFormat::Png).ok()?;
            images::prepare(&first.into_inner(), VIEW_EDGE, 85).ok()
        }
        "image/svg+xml" => None,
        m if m.starts_with("image/") => images::prepare(bytes, VIEW_EDGE, 85).ok(),
        _ => None,
    }
}

pub(crate) async fn result(
    cx: &ToolCx,
    kind: MediaKind,
    provider: &dyn MediaProvider,
    prompt: &str,
    output: Output,
    path: Option<&str>,
) -> Result<ToolOutput, ToolError> {
    let root = cx.attachments.clone().ok_or_else(|| ToolError::failed("This session can't keep generated media"))?;
    let session = cx.sessions.first().cloned().ok_or_else(|| ToolError::failed("This session can't keep generated media"))?;
    let (cx2, prompt, path) = (cx.clone(), prompt.to_owned(), path.map(str::to_owned));
    let media = output.media.clone();
    let stored = tokio::task::spawn_blocking(move || -> Result<Stored, ToolError> {
        let dir = attachments::dir(&root, &session);
        let (slug, n) = (save::slug(&prompt, noun(kind)), media.len());
        let (mut lines, mut meta, mut parts) = (Vec::new(), Vec::new(), Vec::new());
        let mut saved = Vec::new();
        for (i, item) in media.iter().enumerate() {
            let ext = save::extension(&item.mime);
            let file = if n > 1 { format!("{slug}-{}.{ext}", i + 1) } else { format!("{slug}.{ext}") };
            let stored = attachments::put(&dir, &file, &item.mime, &item.bytes)
                .map_err(|e| ToolError::failed(format!("can't keep the {}: {e}", noun(kind))))?;
            let (size, seconds) = measure(&item.bytes, &item.mime);
            lines.push(line(&stored, size, seconds));
            let mut entry = json!({"id": stored.id, "name": stored.name, "mime": stored.mime, "size": stored.size});
            if let Some((w, h)) = size {
                entry["width"] = w.into();
                entry["height"] = h.into();
            }
            if let Some(seconds) = seconds {
                entry["duration"] = seconds.into();
            }
            if let Some(raw) = &path {
                let target = save::target(&cx2, raw, i, n, &item.mime)?;
                save::save(&target, &item.bytes, &item.mime)?;
                entry["path"] = json!(target);
                saved.push(format!("Saved to {}", save::shown(&cx2, &target)));
            }
            meta.push(entry);
            if cx2.vision
                && let Some(view) = view(&item.bytes, &item.mime)
            {
                parts.push(OutputPart::Image { mime: view.mime.into(), data_base64: attachments::encode(&view.bytes) });
            }
        }
        lines.extend(saved);
        Ok((lines, meta, parts))
    })
    .await
    .map_err(|e| ToolError::failed(e.to_string()))??;
    let (lines, meta, parts) = stored;
    let count = output.media.len();
    let what = match (kind, count) {
        (MediaKind::Audio, 1) => "audio file".to_owned(),
        (MediaKind::Audio, _) => "audio files".to_owned(),
        (_, 1) => noun(kind).to_owned(),
        _ => format!("{}s", noun(kind)),
    };
    let mut text = format!("Generated {count} {what} with {} {}.", provider.name(), output.model);
    for line in lines.iter().chain(&output.notes) {
        text.push('\n');
        text.push_str(line);
    }
    let mut metadata = json!({"provider": provider.id(), "model": output.model, "media": meta});
    if let Some(revised) = &output.revised_prompt {
        metadata["revised_prompt"] = revised.as_str().into();
    }
    let mut content = vec![OutputPart::Text { text }];
    content.extend(parts);
    Ok(ToolOutput { content, ..ToolOutput::default() }.with_metadata(metadata))
}
