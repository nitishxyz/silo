//! File choosers a page opens (`<input type=file>`, `showOpenFilePicker`,
//! a save picker): the app shows the system's panel
//! (`Event::FileDialog` → `Browser::file_dialog_done`).

#![allow(clippy::transmute_ptr_to_ptr)]

use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cef::rc::Rc as _;
use cef::*;

use crate::shared::{Event, Shared, lock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogMode {
    /// One existing file.
    Open,
    /// One or more existing files.
    OpenMultiple,
    /// A folder (`webkitdirectory`).
    OpenFolder,
    /// Where to save a file.
    Save,
}

/// A file chooser waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDialog {
    pub id: u32,
    pub mode: FileDialogMode,
    /// The page's title for the panel, if any.
    pub title: Option<String>,
    /// The folder and/or file name to start at.
    pub default_path: Option<PathBuf>,
    /// File extensions the page accepts, lowercase and without the dot;
    /// `None`: any file.
    pub accept: Option<Vec<String>>,
}

impl FileDialog {
    /// The folder [`Self::default_path`] names, if it names one.
    #[must_use]
    pub fn directory(&self) -> Option<PathBuf> {
        let path = self.default_path.as_ref()?;
        if path.is_dir() {
            return Some(path.clone());
        }
        path.parent().filter(|p| !p.as_os_str().is_empty() && p.is_dir()).map(Path::to_path_buf)
    }

    /// The file name [`Self::default_path`] suggests, if any.
    #[must_use]
    pub fn file_name(&self) -> Option<String> {
        let path = self.default_path.as_ref().filter(|p| !p.is_dir())?;
        path.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
    }
}

/// The extensions a chooser allows: `filters` are the page's `accept`
/// values (`.png`, `image/*`, `text/plain`), `extensions` each one's
/// expansion (`.png;.jpg`, empty when unknown). `None` (any file) when
/// there are no filters or one can't be expanded: a type the panel can't
/// express is never made impossible to pick.
#[must_use]
pub fn accept_extensions(filters: &[String], extensions: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for (i, filter) in filters.iter().map(|f| f.trim()).enumerate() {
        if filter.is_empty() {
            continue;
        }
        let found: Vec<String> = if filter.starts_with('.') {
            vec![filter.to_owned()]
        } else {
            extensions.get(i).map(|e| e.split(';').map(str::to_owned).collect()).unwrap_or_default()
        };
        let found: Vec<String> = found
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty() && !e.contains(['/', '*']))
            .collect();
        if found.is_empty() {
            return None;
        }
        for ext in found {
            if !out.contains(&ext) {
                out.push(ext);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn list(value: Option<&mut CefStringList>) -> Vec<String> {
    // A borrowed list: take it (cloning copies the handle, see handlers.rs).
    value.map(|v| std::mem::take(v).into_iter().collect()).unwrap_or_default()
}

wrap_dialog_handler! {
    pub(crate) struct PageDialogs {
        shared: Arc<Shared>,
    }

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: cef::FileDialogMode,
            title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            accept_filters: Option<&mut CefStringList>,
            accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> c_int {
            let Some(callback) = callback else { return 0 };
            // Agents' pages never get a panel.
            if self.shared.deny_prompts.load(Ordering::Acquire) {
                callback.cancel();
                return 1;
            }
            let mode = match mode {
                cef::FileDialogMode::OPEN_MULTIPLE => FileDialogMode::OpenMultiple,
                cef::FileDialogMode::OPEN_FOLDER => FileDialogMode::OpenFolder,
                cef::FileDialogMode::SAVE => FileDialogMode::Save,
                _ => FileDialogMode::Open,
            };
            let text = |s: Option<&CefString>| s.map(ToString::to_string).filter(|s| !s.trim().is_empty());
            let filters = list(accept_filters);
            let extensions = list(accept_extensions);
            let id = self.shared.next_dialog.fetch_add(1, Ordering::Relaxed);
            let dialog = FileDialog {
                id,
                mode,
                title: text(title),
                default_path: text(default_file_path).map(PathBuf::from),
                accept: crate::dialog::accept_extensions(&filters, &extensions),
            };
            let _old = lock(&self.shared.file_dialogs).insert(id, callback.clone());
            self.shared.push(Event::FileDialog(dialog));
            1
        }
    }
}

pub(crate) fn handler(shared: &Arc<Shared>) -> DialogHandler {
    PageDialogs::new(Arc::clone(shared))
}

/// Answer chooser `id` with `paths`, or cancel it (`None`, or no paths).
pub(crate) fn answer(shared: &Shared, id: u32, paths: Option<&[PathBuf]>) {
    let Some(callback) = lock(&shared.file_dialogs).remove(&id) else { return };
    match paths.filter(|p| !p.is_empty()) {
        Some(paths) => {
            let mut list = CefStringList::new();
            for path in paths {
                list.append(&path.to_string_lossy());
            }
            callback.cont(Some(&mut list));
        }
        None => callback.cancel(),
    }
}

/// Cancel every open chooser (the page closed).
pub(crate) fn cancel_all(shared: &Shared) {
    let open: Vec<_> = lock(&shared.file_dialogs).drain().map(|(_, c)| c).collect();
    for callback in open {
        callback.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn accept_filters_become_extensions() {
        assert_eq!(accept_extensions(&[], &[]), None, "no accept: any file");
        assert_eq!(accept_extensions(&strings(&[".PNG", ".jpg"]), &strings(&["", ""])), Some(strings(&["png", "jpg"])));
        assert_eq!(
            accept_extensions(&strings(&["image/*", ".png"]), &strings(&[".png;.jpg;.gif", ""])),
            Some(strings(&["png", "jpg", "gif"])),
            "MIME types expand, duplicates go"
        );
        assert_eq!(accept_extensions(&strings(&["text/plain"]), &strings(&[" .txt ; .text "])), Some(strings(&["txt", "text"])));
        assert_eq!(accept_extensions(&strings(&["application/x-unknown", ".pdf"]), &strings(&["", ""])), None, "an unknown type allows any file");
        assert_eq!(accept_extensions(&strings(&["video/*"]), &[]), None, "no expansion at all");
        assert_eq!(accept_extensions(&strings(&["  "]), &strings(&[""])), None);
    }

    #[test]
    fn default_path_splits_into_folder_and_name() {
        let dir = std::env::temp_dir();
        let dialog = |path: Option<PathBuf>| FileDialog { id: 1, mode: FileDialogMode::Save, title: None, default_path: path, accept: None };
        let named = dialog(Some(dir.join("report.csv")));
        assert_eq!((named.directory(), named.file_name().as_deref()), (Some(dir.clone()), Some("report.csv")));
        let folder = dialog(Some(dir.clone()));
        assert_eq!((folder.directory(), folder.file_name()), (Some(dir), None));
        let bare = dialog(Some(PathBuf::from("notes.txt")));
        assert_eq!((bare.directory(), bare.file_name().as_deref()), (None, Some("notes.txt")));
        assert_eq!(dialog(None).directory(), None);
    }
}
