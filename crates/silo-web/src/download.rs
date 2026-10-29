//! Downloads of a page: a link the server sends as a file, an ⌥-click, or
//! Save … As (`Browser::save_as`). The app picks each file's path
//! (`Event::DownloadAsked` → `Browser::download_to`) and follows progress
//! (`Event::Download`). Nothing is ever opened by itself.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::*;

use crate::shared::{Event, Shared, lock};

/// A download waiting for its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub id: u32,
    pub url: String,
    /// The file name the server or URL suggests (a bare name, never a path).
    pub name: String,
    pub mime: String,
    /// Asked for with Save … As: the user picks where.
    pub save_as: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Running,
    Done,
    Cancelled,
    Failed,
}

/// A download's progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub id: u32,
    /// Where it is written (`None` until its path is chosen).
    pub path: Option<PathBuf>,
    pub received: u64,
    /// `None`: the server sent no size.
    pub total: Option<u64>,
    pub state: DownloadState,
}

/// The engine's answers for one download.
#[derive(Default)]
pub(crate) struct Pending {
    before: Option<BeforeDownloadCallback>,
    item: Option<DownloadItemCallback>,
}

/// `suggested` as a file name: its last path component, without leading
/// dots or control characters; `download` when nothing is left.
#[must_use]
pub fn file_name(suggested: &str) -> String {
    let last = suggested.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control() && *c != ':').collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    if clean.is_empty() { "download".to_owned() } else { clean.to_owned() }
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`… when that path exists or is
/// `taken` (another download is writing it), as Chrome names them.
#[must_use]
pub fn unique_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let free = |path: &Path| !path.exists() && !taken(path);
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => (stem, format!(".{ext}")),
        None => (name, String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|path| free(path)).unwrap_or(first)
}

/// Files macOS runs or installs when opened: Silo never offers Open for
/// them (Chrome's dangerous types on a Mac, minimally).
#[must_use]
pub fn dangerous(name: &str) -> bool {
    const KINDS: &[&str] = &[
        "app",
        "command",
        "dmg",
        "pkg",
        "mpkg",
        "sh",
        "bash",
        "zsh",
        "csh",
        "tool",
        "terminal",
        "scpt",
        "applescript",
        "workflow",
        "jar",
        "py",
        "pl",
        "rb",
        "dylib",
        "kext",
        "prefpane",
        "webloc",
        "inetloc",
        "fileloc",
        "osax",
        "action",
        "definition",
        "exe",
        "msi",
        "bat",
        "dll",
        "scr",
        "js",
        "url",
    ];
    let ext = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    KINDS.contains(&ext.as_str())
}

/// The user's Downloads folder.
#[must_use]
pub fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Downloads")
}

fn text(value: &CefStringUserfree) -> String {
    CefString::from(value).to_string()
}

wrap_download_handler! {
    pub(crate) struct PageDownloads {
        shared: Arc<Shared>,
    }

    impl DownloadHandler {
        // Pages may download (the default here, 0, would refuse every one).
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>, _request_method: Option<&CefString>) -> c_int {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> c_int {
            let (Some(item), Some(callback)) = (download_item, callback) else { return 0 };
            let (id, url, original) = (item.id(), text(&item.url()), text(&item.original_url()));
            let mut name = suggested_name.map(ToString::to_string).unwrap_or_default();
            let save_as = {
                let mut asked = lock(&self.shared.save_as);
                match asked.iter().position(|(u, _)| *u == original || *u == url) {
                    Some(at) => {
                        if let Some(named) = asked.remove(at).1 {
                            name = named;
                        }
                        true
                    }
                    None => false,
                }
            };
            let _old = lock(&self.shared.downloads).entry(id).or_default().before.replace(callback.clone());
            let request = DownloadRequest { id, url, name: file_name(&name), mime: text(&item.mime_type()), save_as };
            self.shared.push(Event::DownloadAsked(request));
            1
        }

        fn on_download_updated(&self, _browser: Option<&mut Browser>, download_item: Option<&mut DownloadItem>, callback: Option<&mut DownloadItemCallback>) {
            let Some(item) = download_item else { return };
            let id = item.id();
            let state = if item.is_complete() != 0 {
                DownloadState::Done
            } else if item.is_canceled() != 0 {
                DownloadState::Cancelled
            } else if item.is_interrupted() != 0 {
                DownloadState::Failed
            } else {
                DownloadState::Running
            };
            // Dropped after the lock: releasing a callback may call back in.
            let _ended = {
                let mut downloads = lock(&self.shared.downloads);
                if state == DownloadState::Running {
                    if let Some(callback) = callback {
                        downloads.entry(id).or_default().item = Some(callback.clone());
                    }
                    None
                } else {
                    downloads.remove(&id)
                }
            };
            let path = Some(text(&item.full_path())).filter(|p| !p.is_empty()).map(PathBuf::from);
            let total = u64::try_from(item.total_bytes()).ok().filter(|&t| t > 0);
            let received = u64::try_from(item.received_bytes()).unwrap_or(0);
            self.shared.push(Event::Download(DownloadProgress { id, path, received, total, state }));
        }
    }
}

pub(crate) fn handler(shared: &Arc<Shared>) -> DownloadHandler {
    PageDownloads::new(Arc::clone(shared))
}

/// Continue download `id` into `path`, or cancel it (`None`). CEF may call
/// back into the handler at once, so no lock is held while it runs.
pub(crate) fn answer(shared: &Shared, id: u32, path: Option<&Path>) {
    let (before, item) = {
        let mut downloads = lock(&shared.downloads);
        let Some(before) = downloads.get_mut(&id).and_then(|p| p.before.take()) else { return };
        let item = if path.is_none() { downloads.remove(&id).and_then(|p| p.item) } else { None };
        (before, item)
    };
    match path {
        Some(path) => before.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0),
        None => {
            if let Some(item) = item {
                item.cancel();
            }
            // Dropping the unanswered callback cancels the download.
            drop(before);
        }
    }
}

/// Cancel a running download.
pub(crate) fn cancel(shared: &Shared, id: u32) {
    let item = lock(&shared.downloads).get_mut(&id).and_then(|p| p.item.take());
    if let Some(item) = item {
        item.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_bare_and_safe() {
        assert_eq!(file_name("report.pdf"), "report.pdf");
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("a\\b.txt"), "b.txt");
        assert_eq!(file_name(".hidden"), "hidden");
        assert_eq!(file_name("  "), "download");
        assert_eq!(file_name("x\u{7}y:z"), "xyz");
    }

    #[test]
    fn unique_paths_count_up_like_chrome() {
        let dir = std::env::temp_dir().join(format!("silo-web-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.pdf", |_| false), dir.join("a.pdf"));
        std::fs::write(dir.join("a.pdf"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.pdf", |_| false), dir.join("a (2).pdf"));
        let taken = dir.join("a (2).pdf");
        assert_eq!(unique_path(&dir, "a.pdf", |p| p == taken), dir.join("a (3).pdf"), "another download writes (2)");
        std::fs::write(dir.join("Makefile"), b"").unwrap();
        assert_eq!(unique_path(&dir, "Makefile", |_| false), dir.join("Makefile (2)"));
        assert_eq!(unique_path(&dir, "x.tar.gz", |_| false), dir.join("x.tar.gz"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installers_and_scripts_are_dangerous() {
        for name in ["Setup.pkg", "tool.COMMAND", "x.dmg", "run.sh", "A.app"] {
            assert!(dangerous(name), "{name}");
        }
        for name in ["report.pdf", "photo.png", "archive.zip", "README"] {
            assert!(!dangerous(name), "{name}");
        }
    }
}
