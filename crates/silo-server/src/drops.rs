//! Uploads: files sent by clients land in
//! `<home>/drops/<`[`silo_proto::drops::name`]`>/<name>`; drops older than
//! [`MAX_AGE`] are removed at server start and daily.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use silo_proto::{
    Frame,
    drops::human_size,
    kind,
    upload::{MAX_ENTRIES, MAX_UPLOAD, UploadEntry, Uploaded},
};
use tokio::{io::AsyncWriteExt, sync::mpsc::Receiver};

/// Drops older than this are removed.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Largest total size of `<home>/drops`; uploads that would exceed it are refused.
pub const MAX_TOTAL: u64 = 5 * 1024 * 1024 * 1024;
/// Longest kept file name, in bytes.
const MAX_NAME: usize = 200;

/// Unicode format characters (category Cf): bidi overrides, zero-width
/// characters and the like, which can disguise a name.
fn is_format(c: char) -> bool {
    matches!(
        c as u32,
        0xAD | 0x600..=0x605
            | 0x61C
            | 0x6DD
            | 0x70F
            | 0x890..=0x891
            | 0x8E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// A single path component safe to create: no separators, control or
/// format characters, `.`/`..` or empty names.
#[must_use]
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name.chars().filter(|&c| !is_format(c)).map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let mut cleaned = cleaned.trim().to_owned();
    if cleaned.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') { "_".to_owned() } else { cleaned }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Remove drops whose stamp is older than `max_age` before `now`; returns
/// how many were removed. Unrecognized entries are left alone.
pub fn gc(home: &Path, now: u64, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(silo_proto::paths::drops(home)) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(at) = name.to_str().and_then(silo_proto::drops::parse) else { continue };
        if now.saturating_sub(at) > max_age.as_secs() && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove expired drops.
pub fn gc_now(home: &Path) {
    let removed = gc(home, now_secs(), MAX_AGE);
    if removed > 0 {
        tracing::info!(removed, "removed old drops");
    }
}

/// Remove expired drops now and then daily.
pub async fn gc_daily(home: PathBuf) {
    let mut daily = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
    loop {
        daily.tick().await;
        let home = home.clone();
        let _ = tokio::task::spawn_blocking(move || gc_now(&home)).await;
    }
}

/// Bytes of the files under `dir`.
fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => size_of(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// A rejected upload: `(code, reason)`.
pub type Refusal = (&'static str, String);

pub fn refuse(code: &'static str, reason: impl Into<String>) -> Refusal {
    (code, reason.into())
}

pub fn io(error: std::io::Error) -> Refusal {
    refuse("io", error.to_string())
}

/// Receive an upload of `total` bytes from `frames` (after the hello) into
/// a fresh drop directory under `home`. On failure the directory is
/// removed.
pub async fn receive(home: &Path, total: u64, frames: &mut Receiver<std::io::Result<Frame>>) -> Result<Uploaded, Refusal> {
    if total > MAX_UPLOAD {
        return Err(refuse("too_large", format!("Too large to upload ({}, max {})", human_size(total), human_size(MAX_UPLOAD))));
    }
    let drops = silo_proto::paths::drops(home);
    let used = tokio::task::spawn_blocking({
        let drops = drops.clone();
        move || size_of(&drops)
    })
    .await
    .unwrap_or(0);
    if used.saturating_add(total) > MAX_TOTAL {
        return Err(refuse(
            "full",
            format!("Not enough upload space ({} of {} used; uploads are removed after 7 days)", human_size(used), human_size(MAX_TOTAL)),
        ));
    }
    tokio::fs::create_dir_all(&drops).await.map_err(io)?;
    let dir = drops.join(silo_proto::drops::name());
    tokio::fs::create_dir(&dir).await.map_err(io)?;
    let dir = tokio::fs::canonicalize(&dir).await.map_err(io)?;
    let mut created = Vec::new();
    let result = receive_into(&dir, total, frames, &mut created, |dir, name| Ok(unique(dir, &sanitize(name)))).await;
    if result.is_err() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    result.map(|()| Uploaded { paths: created.iter().map(|p| p.to_string_lossy().into_owned()).collect() })
}

/// Receive an upload's entries (after the hello) into `dir`: each
/// top-level item lands where `place` says (given the folder and the name
/// as sent) and is added to `created`; deeper entries go inside it,
/// sanitized.
pub async fn receive_into(
    dir: &Path,
    total: u64,
    frames: &mut Receiver<std::io::Result<Frame>>,
    created: &mut Vec<PathBuf>,
    place: impl Fn(&Path, &str) -> Result<PathBuf, Refusal>,
) -> Result<(), Refusal> {
    // The current top-level item: its name as sent and where it landed.
    let mut top: Option<(String, PathBuf, bool)> = None;
    let mut received = 0u64;
    let mut entries = 0usize;
    loop {
        let frame = match frames.recv().await {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => return Err(io(e)),
            None => return Err(refuse("closed", "upload ended early")),
        };
        match frame.kind {
            kind::UPLOAD_END => return Ok(()),
            kind::UPLOAD_ENTRY => {}
            other => return Err(refuse("protocol", format!("unexpected upload frame {other:#x}"))),
        }
        let entry: UploadEntry = frame.parse().map_err(|e| refuse("protocol", e.to_string()))?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(refuse("too_many", format!("Too many files to upload (max {MAX_ENTRIES})")));
        }
        let target = match entry.path.as_slice() {
            [] => return Err(refuse("protocol", "empty path")),
            [name] => {
                let path = place(dir, name)?;
                created.push(path.clone());
                top = Some((name.clone(), path.clone(), entry.dir));
                path
            }
            [first, rest @ ..] => match &top {
                Some((name, root, true)) if name == first => rest.iter().fold(root.clone(), |p, c| p.join(sanitize(c))),
                _ => return Err(refuse("protocol", "entry outside the current directory")),
            },
        };
        if entry.dir {
            tokio::fs::create_dir_all(&target).await.map_err(io)?;
            continue;
        }
        if entry.size > total - received {
            return Err(refuse("too_large", "upload is larger than announced"));
        }
        let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&target).await.map_err(io)?;
        let mut left = entry.size;
        while left > 0 {
            let chunk = match frames.recv().await {
                Some(Ok(frame)) if frame.kind == kind::UPLOAD_CHUNK => frame.payload,
                Some(Ok(frame)) => return Err(refuse("protocol", format!("expected file data, got {:#x}", frame.kind))),
                Some(Err(e)) => return Err(io(e)),
                None => return Err(refuse("closed", "upload ended early")),
            };
            if chunk.len() as u64 > left {
                return Err(refuse("protocol", "file data exceeds its size"));
            }
            file.write_all(&chunk).await.map_err(io)?;
            left -= chunk.len() as u64;
        }
        file.flush().await.map_err(io)?;
        received += entry.size;
    }
}

/// `dir/name`, or `dir/name (n)` when taken.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    (2..).map(|n| dir.join(format!("{name} ({n})"))).find(|p| !p.exists()).expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("."), "_");
        assert_eq!(sanitize("  "), "_");
        assert_eq!(sanitize("x\ny\0"), "x_y_");
        assert_eq!(sanitize(".env"), ".env");
        assert_eq!(sanitize(&"é".repeat(300)).len(), MAX_NAME);
        assert_eq!(sanitize("evil\u{202E}gnp.exe"), "evilgnp.exe");
        assert_eq!(sanitize("a\u{200B}b\u{FEFF}"), "ab");
    }

    #[test]
    fn gc_removes_only_expired_drops() {
        let home = std::env::temp_dir().join(format!("silo-drops-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let drops = silo_proto::paths::drops(&home);
        let now = 1_792_454_400;
        let stamp = silo_proto::drops::stamp;
        let old = drops.join(format!("{}-aaaaaa", stamp(now - 8 * 86_400)));
        let fresh = drops.join(format!("{}-bbbbbb", stamp(now - 6 * 86_400)));
        let other = drops.join("keep-me");
        for dir in [&old, &fresh, &other] {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/f"), b"x").unwrap();
        }
        assert_eq!(gc(&home, now, MAX_AGE), 1);
        assert!(!old.exists() && fresh.exists() && other.exists());
        assert_eq!(size_of(&drops), 2);
        let _ = std::fs::remove_dir_all(&home);
    }
}
