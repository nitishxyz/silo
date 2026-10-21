//! Silo's own clipboard: programs in a block read and write the user's
//! clipboard through the server, and files or images reach a block by
//! drop or ⌘V. Files are uploaded to the block's host, on this Mac too:
//! a drag lends access to this app alone, and programs in a block (run by
//! the server, a separate process for macOS privacy) often can't read the
//! originals, so they get a copy in a drop directory.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use gpui::{ClipboardEntry, ClipboardItem, ImageFormat};
use silo_proto::data::{ClientData, Clip, ClipKind, MAX_CLIP_BYTES};

/// How long after a paste gesture a program may read the clipboard.
pub const GESTURE_WINDOW: Duration = Duration::from_secs(3);

/// Whether a clipboard read may be answered: the block is focused here and
/// the user asked to paste into it moments ago.
#[must_use]
pub fn read_allowed(focused: bool, gesture: Option<Instant>, now: Instant) -> bool {
    focused && gesture.is_some_and(|at| now.saturating_duration_since(at) <= GESTURE_WINDOW)
}

/// A paste gesture: ⌘V, Ctrl+V or Ctrl+Shift+V.
#[must_use]
pub fn is_paste_gesture(key: &str, modifiers: &gpui::Modifiers) -> bool {
    key.eq_ignore_ascii_case("v") && !modifiers.alt && (modifiers.platform != modifiers.control) && (!modifiers.platform || !modifiers.shift)
}

/// `path` as one POSIX shell word, backslash-escaped like Terminal, iTerm
/// and Ghostty paste dropped files (TUIs detect dropped paths in that
/// form). A newline can't be backslash-escaped, so such a path is quoted.
#[must_use]
pub fn shell_quote(path: &str) -> String {
    if path.is_empty() {
        return "''".to_owned();
    }
    if path.contains('\n') {
        return format!("'{}'", path.replace('\'', r"'\''"));
    }
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii() && !c.is_ascii_alphanumeric() && !"/._-+,:@%=".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[must_use]
pub fn quoted<S: AsRef<str>>(paths: &[S]) -> String {
    paths.iter().map(|p| shell_quote(p.as_ref())).collect::<Vec<_>>().join(" ")
}

/// What the clipboard holds, as far as pasting into a terminal goes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Contents {
    pub paths: Vec<PathBuf>,
    pub text: Option<String>,
    pub image: Option<(ImageFormat, Vec<u8>)>,
}

impl Contents {
    #[must_use]
    pub fn of(item: &ClipboardItem) -> Self {
        let mut contents = Self::default();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::ExternalPaths(paths) if contents.paths.is_empty() => contents.paths = paths.paths().to_vec(),
                ClipboardEntry::String(s) if contents.text.is_none() => contents.text = Some(s.text().clone()),
                ClipboardEntry::Image(image) if contents.image.is_none() => contents.image = Some((image.format(), image.bytes().to_vec())),
                _ => {}
            }
        }
        contents
    }
}

/// Something to put on the block's host before pasting its path.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Path(PathBuf),
    /// The clipboard image as PNG, stored as `clipboard.png`.
    Png(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Paste {
    Text(String),
    /// Save the image (PNG) locally, then paste its path.
    SaveImage(Vec<u8>),
    Upload(Vec<Source>),
}

/// Files dropped or pasted into a block: uploaded wherever it runs.
#[must_use]
pub fn files(paths: Vec<PathBuf>) -> Vec<Source> {
    paths.into_iter().map(Source::Path).collect()
}

/// What ⌘V does with `contents`: files beat text (Finder also puts their
/// names there), text beats an image.
#[must_use]
pub fn choose(contents: Contents, remote: bool) -> Option<Paste> {
    if !contents.paths.is_empty() {
        return Some(Paste::Upload(files(contents.paths)));
    }
    if let Some(text) = contents.text {
        return Some(Paste::Text(text));
    }
    let png = contents.image.and_then(|(format, bytes)| png(format, bytes))?;
    Some(if remote { Paste::Upload(vec![Source::Png(png)]) } else { Paste::SaveImage(png) })
}

/// Image bytes as PNG (macOS often offers only TIFF).
#[must_use]
pub fn png(format: ImageFormat, bytes: Vec<u8>) -> Option<Vec<u8>> {
    let format = match format {
        ImageFormat::Png => return Some(bytes),
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        _ => return None,
    };
    let decoded = image::load_from_memory_with_format(&bytes, format).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// `<home>/drops/<drop name>/clipboard.png` with `png` written to it.
///
/// # Errors
/// The directory or file cannot be written.
pub fn save_png(home: &Path, png: &[u8]) -> std::io::Result<PathBuf> {
    let dir = silo_proto::paths::drops(home).join(silo_proto::drops::name());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("clipboard.png");
    std::fs::write(&path, png)?;
    Ok(path)
}

/// The answer to a program's clipboard read.
#[must_use]
pub fn answer(contents: Contents, kind: ClipKind) -> Option<Clip> {
    match kind {
        ClipKind::Text => contents.text.filter(|t| t.len() <= MAX_CLIP_BYTES).map(Clip::Text),
        ClipKind::Image => {
            let (format, bytes) = contents.image?;
            let data = png(format, bytes).filter(|d| d.len() <= MAX_CLIP_BYTES)?;
            Some(Clip::Image { mime: "image/png".into(), data })
        }
        ClipKind::Types => {
            let mut types = Vec::new();
            if !contents.paths.is_empty() {
                types.push("text/uri-list".to_owned());
            }
            if contents.text.is_some() {
                types.extend(["text/plain;charset=utf-8".to_owned(), "text/plain".to_owned()]);
            }
            if contents.image.is_some() {
                types.push("image/png".to_owned());
            }
            Some(Clip::Types(types))
        }
    }
}

/// What a paste gesture in a remote block puts on that host's clipboard:
/// text, else an image as PNG. Files stay here (they are uploaded).
#[must_use]
pub fn push(contents: Contents) -> Option<Clip> {
    if !contents.paths.is_empty() {
        return None;
    }
    if let Some(text) = contents.text {
        return (text.len() <= MAX_CLIP_BYTES).then_some(Clip::Text(text));
    }
    let (format, bytes) = contents.image?;
    let data = png(format, bytes).filter(|d| d.len() <= MAX_CLIP_BYTES)?;
    Some(Clip::Image { mime: "image/png".into(), data })
}

/// Frames for the data stream: a gesture's clipboard push always precedes
/// the input it accompanies, so the host has the clipboard when the paste
/// is processed.
#[must_use]
pub fn frames(push: Option<Clip>, input: Option<Vec<u8>>) -> Vec<ClientData> {
    push.map(|content| ClientData::ClipboardPush { content }).into_iter().chain(input.map(ClientData::Input)).collect()
}

#[must_use]
pub fn copied_label(remote_host: Option<&str>) -> String {
    remote_host.map_or_else(|| "Copied".into(), |host| format!("Copied from {host}"))
}

#[must_use]
pub fn upload_label(files: usize, sent: u64, total: u64) -> String {
    let percent = sent.min(total).saturating_mul(100).checked_div(total).unwrap_or(100);
    format!("Uploading {files} file{} · {percent}%", if files == 1 { "" } else { "s" })
}

/// What a failed upload tells the user: the limit it hit, or why a file
/// couldn't be read, never a path.
#[must_use]
pub fn upload_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if ["Too large to upload", "Too many files to upload", "Not enough upload space"].iter().any(|p| message.starts_with(p)) {
        return message;
    }
    match error.chain().find_map(|e| e.downcast_ref::<std::io::Error>()).map(std::io::Error::kind) {
        Some(std::io::ErrorKind::PermissionDenied) => "Silo can't read a dropped file".into(),
        Some(std::io::ErrorKind::NotFound) => "A dropped file is gone".into(),
        _ => "Upload failed".into(),
    }
}

/// Clipboard writes from blocks. Every window showing a block receives its
/// writes: one applies it, and a remote write that no window may apply yet
/// waits until the block is focused.
#[derive(Default)]
pub struct Writes {
    pending: HashMap<String, String>,
    applied: Option<(String, String, Instant)>,
}

/// A write repeated within this long is the same write seen by another window.
const SAME_WRITE: Duration = Duration::from_secs(2);

impl Writes {
    fn seen(&self, block: &str, text: &str, now: Instant) -> bool {
        self.applied.as_ref().is_some_and(|(b, t, at)| b == block && t == text && now.saturating_duration_since(*at) < SAME_WRITE)
    }

    /// Whether to apply `text` now (false: another window already did).
    pub fn apply(&mut self, block: &str, text: &str, now: Instant) -> bool {
        if self.seen(block, text, now) {
            return false;
        }
        self.pending.remove(block);
        self.applied = Some((block.to_owned(), text.to_owned(), now));
        true
    }

    /// Hold `text` as the block's latest write until it is focused.
    pub fn defer(&mut self, block: &str, text: String, now: Instant) {
        if !self.seen(block, &text, now) {
            self.pending.insert(block.to_owned(), text);
        }
    }

    /// The write held for a block just focused, to apply now.
    pub fn take(&mut self, block: &str, now: Instant) -> Option<String> {
        let text = self.pending.remove(block)?;
        self.applied = Some((block.to_owned(), text.clone(), now));
        Some(text)
    }
}

pub fn writes<R>(f: impl FnOnce(&mut Writes) -> R) -> R {
    static WRITES: Mutex<Option<Writes>> = Mutex::new(None);
    f(WRITES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(Writes::default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_need_focus_and_a_recent_gesture() {
        let now = Instant::now();
        let recent = now.checked_sub(Duration::from_secs(1));
        let stale = now.checked_sub(Duration::from_secs(4));
        assert!(read_allowed(true, recent, now));
        assert!(!read_allowed(false, recent, now));
        assert!(!read_allowed(true, stale, now));
        assert!(!read_allowed(true, None, now));
    }

    #[test]
    fn paste_gestures() {
        let m = |platform, control, shift| gpui::Modifiers { platform, control, shift, ..Default::default() };
        assert!(is_paste_gesture("v", &m(true, false, false)));
        assert!(is_paste_gesture("v", &m(false, true, false)));
        assert!(is_paste_gesture("v", &m(false, true, true)));
        assert!(!is_paste_gesture("v", &m(false, false, false)));
        assert!(!is_paste_gesture("v", &m(true, true, false)));
        assert!(!is_paste_gesture("c", &m(false, true, false)));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/tmp/a.txt"), "/tmp/a.txt");
        assert_eq!(shell_quote("/tmp/my file"), r"/tmp/my\ file");
        assert_eq!(shell_quote("/tmp/it's"), r"/tmp/it\'s");
        assert_eq!(shell_quote("$HOME;rm"), r"\$HOME\;rm");
        assert_eq!(shell_quote("/tmp/café 1.png"), r"/tmp/café\ 1.png");
        assert_eq!(shell_quote("/a\nb"), "'/a\nb'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(quoted(&["/a", "/b c"]), r"/a /b\ c");
    }

    fn contents(paths: &[&str], text: Option<&str>, image: bool) -> Contents {
        Contents {
            paths: paths.iter().map(PathBuf::from).collect(),
            text: text.map(Into::into),
            image: image.then(|| (ImageFormat::Png, vec![1, 2, 3])),
        }
    }

    #[test]
    fn choose_for_clipboard() {
        let files = || contents(&["/x/a.txt"], Some("a.txt"), false);
        for remote in [false, true] {
            assert_eq!(choose(files(), remote), Some(Paste::Upload(vec![Source::Path("/x/a.txt".into())])), "remote: {remote}");
            assert_eq!(choose(contents(&[], Some("hi"), true), remote), Some(Paste::Text("hi".into())));
            assert_eq!(choose(contents(&[], None, false), remote), None);
        }
        assert_eq!(choose(contents(&[], None, true), false), Some(Paste::SaveImage(vec![1, 2, 3])));
        assert_eq!(choose(contents(&[], None, true), true), Some(Paste::Upload(vec![Source::Png(vec![1, 2, 3])])));
    }

    #[test]
    fn dropped_files_are_uploaded() {
        let shot = PathBuf::from("/var/folders/x/T/TemporaryItems/NSIRD_screencaptureui_1/Shot 1.png");
        assert_eq!(files(vec![shot.clone(), "/x/dir".into()]), vec![Source::Path(shot), Source::Path("/x/dir".into())]);
    }

    #[test]
    fn tiff_becomes_png() {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let mut tiff = std::io::Cursor::new(Vec::new());
        img.write_to(&mut tiff, image::ImageFormat::Tiff).unwrap();
        let png = png(ImageFormat::Tiff, tiff.into_inner()).unwrap();
        assert_eq!(image::guess_format(&png).unwrap(), image::ImageFormat::Png);
    }

    #[test]
    fn saved_images_go_to_a_named_drop() {
        let home = std::env::temp_dir().join(format!("silo-app-drop-{}", std::process::id()));
        let path = save_png(&home, b"png").unwrap();
        let dir = path.parent().unwrap();
        assert_eq!(dir.parent().unwrap(), silo_proto::paths::drops(&home));
        assert!(silo_proto::drops::parse(&dir.file_name().unwrap().to_string_lossy()).is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"png");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn answers() {
        assert_eq!(answer(contents(&[], Some("hi"), false), ClipKind::Text), Some(Clip::Text("hi".into())));
        assert_eq!(answer(contents(&[], None, true), ClipKind::Text), None);
        assert_eq!(answer(contents(&[], None, true), ClipKind::Image), Some(Clip::Image { mime: "image/png".into(), data: vec![1, 2, 3] }));
        assert_eq!(
            answer(contents(&[], Some("hi"), true), ClipKind::Types),
            Some(Clip::Types(vec!["text/plain;charset=utf-8".into(), "text/plain".into(), "image/png".into()]))
        );
    }

    #[test]
    fn pushes_text_or_image_never_files() {
        assert_eq!(push(contents(&[], Some("hi"), false)), Some(Clip::Text("hi".into())));
        assert_eq!(push(contents(&[], None, true)), Some(Clip::Image { mime: "image/png".into(), data: vec![1, 2, 3] }));
        assert_eq!(push(contents(&["/x/a.txt"], Some("a.txt"), false)), None);
        assert_eq!(push(contents(&[], None, false)), None);
        assert_eq!(push(contents(&[], Some(&"x".repeat(MAX_CLIP_BYTES + 1)), false)), None);
    }

    #[test]
    fn push_precedes_the_paste() {
        let clip = || Clip::Text("hi".into());
        assert_eq!(
            frames(Some(clip()), Some(b"\x16".to_vec())),
            vec![ClientData::ClipboardPush { content: clip() }, ClientData::Input(b"\x16".to_vec())]
        );
        assert_eq!(frames(Some(clip()), None), vec![ClientData::ClipboardPush { content: clip() }]);
        assert_eq!(frames(None, Some(b"a".to_vec())), vec![ClientData::Input(b"a".to_vec())]);
        assert!(frames(None, None).is_empty());
    }

    #[test]
    fn labels() {
        assert_eq!(copied_label(None), "Copied");
        assert_eq!(copied_label(Some("studio")), "Copied from studio");
        assert_eq!(upload_label(3, 45, 100), "Uploading 3 files · 45%");
        assert_eq!(upload_label(1, 0, 0), "Uploading 1 file · 100%");
    }

    #[test]
    fn upload_errors_name_the_problem_not_the_path() {
        use anyhow::Context;
        let dir = std::env::temp_dir().join(format!("silo-upload-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let open = |path: &Path| std::fs::File::open(path).with_context(|| format!("open {}", path.display())).unwrap_err();

        let gone = dir.join("gone.png");
        assert_eq!(upload_error(&open(&gone)), "A dropped file is gone");
        let link = dir.join("dangling");
        std::os::unix::fs::symlink(&gone, &link).unwrap();
        assert_eq!(upload_error(&open(&link)), "A dropped file is gone");

        let secret = dir.join("secret.txt");
        std::fs::write(&secret, b"x").unwrap();
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        if std::fs::File::open(&secret).is_err() {
            assert_eq!(upload_error(&open(&secret)), "Silo can't read a dropped file");
        }

        for limit in
            ["Too large to upload (3.0 GB, max 2.0 GB)", "Too many files to upload (max 10000)", "Not enough upload space (5 GB of 5 GB used)"]
        {
            assert_eq!(upload_error(&anyhow::anyhow!("{limit}")), limit);
        }
        let shrank = anyhow::anyhow!("{} shrank during the upload", secret.display());
        assert_eq!(upload_error(&shrank), "Upload failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_apply_once_across_windows() {
        let now = Instant::now();
        let mut w = Writes::default();
        assert!(w.apply("a", "x", now));
        assert!(!w.apply("a", "x", now), "a second window");
        w.defer("a", "x".into(), now);
        assert_eq!(w.take("a", now), None, "already applied");
        assert!(w.apply("b", "x", now));
        assert!(w.apply("a", "x", now + SAME_WRITE), "copied again later");
    }

    #[test]
    fn deferred_writes_wait_for_focus() {
        let now = Instant::now();
        let mut w = Writes::default();
        w.defer("a", "old".into(), now);
        w.defer("a", "new".into(), now);
        assert_eq!(w.take("b", now), None);
        assert_eq!(w.take("a", now).as_deref(), Some("new"));
        assert_eq!(w.take("a", now), None);
        w.defer("a", "new".into(), now);
        assert_eq!(w.take("a", now), None, "a deferral from another window after it was applied");
        w.defer("a", "z".into(), now);
        assert!(w.apply("a", "y", now), "a visible window applies a newer write");
        assert_eq!(w.take("a", now), None);
    }
}
