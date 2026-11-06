//! Files in and out of a generation: references read from the project or the session's attachments, results
//! named from the prompt, stored as attachments and saved into the project.
use crate::Reference;
use otto_tools::{ToolCx, ToolError, ToolErrorKind, attachments, images};
use std::path::{Path, PathBuf};

/// The longest edge a reference is sent at.
const REFERENCE_EDGE: u32 = 2048;

/// `reference` (a project path or an attachment id) as PNG or JPEG for a provider.
pub(crate) fn reference(cx: &ToolCx, reference: &str) -> Result<Reference, ToolError> {
    image(&read(cx, reference)?, reference)
}

/// An image's `bytes` as PNG or JPEG for a provider.
pub(crate) fn image(bytes: &[u8], name: &str) -> Result<Reference, ToolError> {
    let prepared =
        images::prepare(bytes, REFERENCE_EDGE, 90).map_err(|e| ToolError::invalid(format!("{name} is not an image this can send: {e}")))?;
    Ok(Reference { bytes: prepared.bytes, mime: prepared.mime.into() })
}

/// `raw`, a transparent PNG annotation layer drawn over `first` (as read, before it was prepared), at the size
/// `first` is sent.
pub(crate) fn markup(cx: &ToolCx, raw: &str, first: &str) -> Result<Reference, ToolError> {
    let bytes = read(cx, raw)?;
    let layer = image::load_from_memory(&bytes).map_err(|e| ToolError::invalid(format!("markup {raw} is not an image: {e}")))?;
    if images::sniff(&bytes) != Some("image/png") || !layer.to_rgba8().pixels().any(|p| p.0[3] < 255) {
        return Err(ToolError::invalid(format!("markup {raw} must be a PNG with transparent pixels (annotations only, not flattened)")));
    }
    let first = read(cx, first)?;
    let original = images::describe(&first).map(|(d, _)| d);
    let sent = images::prepare(&first, REFERENCE_EDGE, 90).map_err(ToolError::invalid)?.sent;
    let have = (layer.width(), layer.height());
    if original != Some(have) {
        let (w, h) = original.unwrap_or_default();
        return Err(ToolError::invalid(format!("markup {raw} is {}×{}; it must be the first reference's size, {w}×{h}", have.0, have.1)));
    }
    if have == sent {
        return Ok(Reference { bytes, mime: "image/png".into() });
    }
    let scaled = layer.resize_exact(sent.0, sent.1, image::imageops::FilterType::Triangle);
    let mut out = std::io::Cursor::new(Vec::new());
    scaled.write_to(&mut out, image::ImageFormat::Png).map_err(|e| ToolError::failed(e.to_string()))?;
    Ok(Reference { bytes: out.into_inner(), mime: "image/png".into() })
}

/// The bytes of a project path or an attachment id.
pub(crate) fn read(cx: &ToolCx, reference: &str) -> Result<Vec<u8>, ToolError> {
    if attachments::is_id(reference) {
        let root = cx.attachments.as_ref().ok_or_else(|| ToolError::invalid("This session has no attachments"))?;
        let (_, path) = attachments::find(root, &cx.sessions, reference).map_err(|e| ToolError::new(ToolErrorKind::NotFound, e.to_string()))?;
        std::fs::read(path)
    } else {
        let path = otto_tools::policy::resolve_path(&cx.cwd, &cx.project_root, reference)?.path;
        if !path.is_file() {
            return Err(ToolError::new(ToolErrorKind::NotFound, format!("{reference} is not a file")));
        }
        std::fs::read(path)
    }
    .map_err(|e| ToolError::failed(format!("{reference}: {e}")))
}

/// `sunset-over-dunes`: the prompt's first words, lowercase, dash-joined; `fallback` without any.
pub(crate) fn slug(prompt: &str, fallback: &str) -> String {
    let words: Vec<String> = prompt.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).take(6).collect();
    let mut slug = String::new();
    for word in words {
        if !slug.is_empty() && slug.len() + word.len() >= 48 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.chars().take(48).collect::<String>());
    }
    if slug.is_empty() { fallback.into() } else { slug }
}

/// `png`, `jpg`, `webp`, … for a mime.
pub(crate) fn extension(mime: &str) -> &str {
    match mime {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        other => other.rsplit('/').next().filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("bin"),
    }
}

/// Where file `i` of `n` goes for `raw` (`logo.png` → `logo-2.png`), and its extension (`mime`'s when `raw` has none).
pub(crate) fn target(cx: &ToolCx, raw: &str, i: usize, n: usize, mime: &str) -> Result<PathBuf, ToolError> {
    let path = otto_tools::policy::resolve_path(&cx.cwd, &cx.project_root, raw)?.path;
    let ext = path.extension().and_then(|e| e.to_str()).map_or_else(|| extension(mime).to_owned(), str::to_owned);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("media").to_owned();
    let name = if n > 1 { format!("{stem}-{}.{ext}", i + 1) } else { format!("{stem}.{ext}") };
    Ok(path.with_file_name(name))
}

/// Write `bytes` (of `mime`) at `path`, converted when its extension names another raster format; an SVG, or a
/// GIF at a `.gif` path, as is.
pub(crate) fn save(path: &Path, bytes: &[u8], mime: &str) -> Result<(), ToolError> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let svg = (mime == "image/svg+xml", ext.as_deref() == Some("svg"));
    let wanted = ext.as_deref().and_then(image::ImageFormat::from_extension).filter(|_| !(mime == "image/gif" && ext.as_deref() == Some("gif")));
    if (wanted.is_some() && !mime.starts_with("image/")) || svg.0 != svg.1 {
        return Err(ToolError::invalid(format!("can't save {mime} as {}: give path a .{} name", path.display(), extension(mime))));
    }
    let have = image::guess_format(bytes).ok();
    let converted;
    let bytes = match (wanted, have) {
        (Some(wanted), Some(have)) if wanted != have => {
            let decoded = image::load_from_memory(bytes).map_err(|e| ToolError::failed(e.to_string()))?;
            let decoded = if wanted == image::ImageFormat::Jpeg { image::DynamicImage::ImageRgb8(decoded.to_rgb8()) } else { decoded };
            let mut out = std::io::Cursor::new(Vec::new());
            decoded.write_to(&mut out, wanted).map_err(|e| ToolError::failed(format!("can't save {mime} as {}: {e}", path.display())))?;
            converted = out.into_inner();
            converted.as_slice()
        }
        _ => bytes,
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
    if path.is_dir() {
        return Err(ToolError::invalid(format!("{} is a folder", path.display())));
    }
    let tmp = parent.join(format!(".{}.otto-tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("media")));
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::failed(format!("{}: {e}", path.display()))
    })
}

/// `path` as the user sees it: relative to the working directory when inside it.
pub(crate) fn shown(cx: &ToolCx, path: &Path) -> String {
    let cwd = std::fs::canonicalize(&cx.cwd).unwrap_or_else(|_| cx.cwd.clone());
    path.strip_prefix(&cwd).unwrap_or(path).display().to_string()
}
