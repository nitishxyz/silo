//! Media under a studio's root: types by extension (else by their first
//! bytes), image sizes from headers, `studio.asset` and `studio.import`.

use std::{
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

use silo_proto::{
    control::RpcError,
    studio::{StudioAsset, StudioImported, StudioMedia},
};

use super::disk;
use crate::files::paths;

fn by_extension(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

fn sniff(head: &[u8]) -> &'static str {
    let at = |i: usize, magic: &[u8]| head.get(i..i + magic.len()) == Some(magic);
    if at(0, b"\x89PNG") {
        "image/png"
    } else if at(0, b"\xff\xd8\xff") {
        "image/jpeg"
    } else if at(0, b"GIF8") {
        "image/gif"
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        "image/webp"
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        "audio/wav"
    } else if at(0, b"ID3") || at(0, b"\xff\xfb") {
        "audio/mpeg"
    } else if at(4, b"ftypqt") {
        "video/quicktime"
    } else if at(4, b"ftypM4A") {
        "audio/mp4"
    } else if at(4, b"ftyp") {
        "video/mp4"
    } else if at(0, b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else if at(0, b"OggS") {
        "audio/ogg"
    } else if at(0, b"fLaC") {
        "audio/flac"
    } else if at(0, b"%PDF") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}

/// The MIME type of `path`: by extension, else by its first bytes.
pub fn mime_of(path: &Path) -> String {
    if let Some(mime) = path.file_name().and_then(|n| n.to_str()).and_then(by_extension) {
        return mime.to_owned();
    }
    let mut head = [0u8; 16];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    sniff(&head[..n]).to_owned()
}

/// The kind of media at root-relative `rel`, by its name (`.md`,
/// `.markdown`, `.txt`: a text document).
pub fn media_of(rel: &str) -> Option<StudioMedia> {
    match StudioMedia::of_path(rel) {
        StudioMedia::Unknown => None,
        media => Some(media),
    }
}

/// Width and height of the image at root-relative `rel`, from its header;
/// `None` when it can't be read (not there yet, not an image).
pub fn image_size(root: &Path, rel: &str) -> Option<(u32, u32)> {
    let path = paths::resolve(root, rel).ok()?;
    let size = imagesize::size(&path).ok()?;
    Some((u32::try_from(size.width).ok()?, u32::try_from(size.height).ok()?))
}

/// `studio.asset`: a chunk of a file under the root.
///
/// # Errors
/// Path errors; `bad_request` for a folder or an offset past the end.
pub fn asset(root: &Path, rel: &str, offset: Option<u64>, length: Option<u64>) -> Result<StudioAsset, RpcError> {
    let path = paths::resolve(root, rel)?;
    let meta = path.metadata().map_err(|e| disk::io_error(rel, &e))?;
    if !meta.is_file() {
        return Err(RpcError::bad_request(format!("{rel} is not a file")));
    }
    let size = meta.len();
    let offset = offset.unwrap_or(0);
    if offset > size {
        return Err(RpcError::bad_request(format!("offset {offset} is past the end of {rel} ({size} bytes)")));
    }
    let length = length.unwrap_or(StudioAsset::MAX_CHUNK).min(StudioAsset::MAX_CHUNK).min(size - offset);
    let mut file = std::fs::File::open(&path).map_err(|e| disk::io_error(rel, &e))?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| disk::io_error(rel, &e))?;
    let mut data = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(length).read_to_end(&mut data).map_err(|e| disk::io_error(rel, &e))?;
    Ok(StudioAsset { mime: mime_of(&path), size, offset, data_base64: data_encoding::BASE64.encode(&data) })
}

/// The file `from` names for an import: root-relative, or absolute (`~/…`
/// too). Outside the root only for the owner.
pub(super) fn source(root: &Path, from: &str, owner: bool) -> Result<PathBuf, RpcError> {
    let from = from.trim();
    if !(from.starts_with('/') || from == "~" || from.starts_with("~/")) {
        return paths::resolve(root, from);
    }
    let path = match from.strip_prefix('~') {
        Some(rest) => silo_pty::home_dir().join(rest.trim_start_matches('/')),
        None => PathBuf::from(from),
    };
    let path = path.canonicalize().map_err(|e| disk::io_error(from, &e))?;
    if !owner && !paths::inside(&paths::canonical_root(root)?, &path) {
        return Err(RpcError::denied(format!("{from} is outside the studio: only this Mac's user may import from elsewhere")));
    }
    Ok(path)
}

/// `studio.import`: copy `from` into canvas `canvas`'s `assets/` (a taken
/// name gets `-2`, `-3`…); a file already there is answered as it is.
///
/// # Errors
/// `not_found` for a missing canvas or file; `forbidden` for an absolute
/// path outside the root without owner rights.
pub fn import(root: &Path, canvas: &str, from: &str, owner: bool) -> Result<StudioImported, RpcError> {
    let canvas = disk::component("canvas", canvas)?;
    if !root.join("canvases").join(&canvas).join(super::canvas::FILE).is_file() {
        return Err(RpcError::not_found(format!("canvas {canvas}")));
    }
    let src = source(root, from, owner)?;
    if !src.is_file() {
        return Err(RpcError::bad_request(format!("{from} is not a file")));
    }
    let assets_rel = format!("canvases/{canvas}/assets");
    let assets = disk::mkdirs(root, &assets_rel)?.canonicalize().map_err(|e| disk::io_error(&assets_rel, &e))?;
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| RpcError::bad_request("no file name"))?;
    if src.parent() == Some(assets.as_path()) {
        return Ok(StudioImported { path: format!("{assets_rel}/{name}") });
    }
    // One of the canvas's own documents (its script): pinned where it is, not copied.
    if silo_proto::studio::is_document(&name) && src.parent().is_some_and(|p| assets.parent() == Some(p)) {
        return Ok(StudioImported { path: format!("canvases/{canvas}/{name}") });
    }
    let name = copy_into(root, &src, &assets, &assets_rel, &name, from)?;
    Ok(StudioImported { path: format!("{assets_rel}/{name}") })
}

/// Copy `src` into folder `dir` (root-relative `dir_rel`) as `name`, else
/// `name-2`… when taken; the name it got. `from`: for errors.
///
/// # Errors
/// An I/O failure.
pub(super) fn copy_into(root: &Path, src: &Path, dir: &Path, dir_rel: &str, name: &str, from: &str) -> Result<String, RpcError> {
    disk::locked(root, dir_rel, || {
        let name = disk::free_name(dir, name, true);
        let temp = dir.join(format!(".{name}.silo-{}-{:08x}.tmp", std::process::id(), rand::random::<u32>()));
        let copied = std::fs::copy(src, &temp).and_then(|_| std::fs::rename(&temp, dir.join(&name)));
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            return Err(disk::io_error(from, &error));
        }
        Ok(name)
    })
}
