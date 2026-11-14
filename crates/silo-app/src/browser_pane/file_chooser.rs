//! A page's file chooser (`<input type=file>`, a save picker) as the
//! system's open or save panel, a sheet on the key window: one file or
//! several, a folder when asked, the page's accepted types, its starting
//! folder and name. Cancel answers with no files.

use std::cell::Cell;
use std::path::PathBuf;

use objc2::{msg_send, rc::Retained, runtime::AnyClass, runtime::AnyObject, runtime::Bool};
use objc2_foundation::{NSArray, NSString, NSURL};
use silo_web::{FileDialog, FileDialogMode};

use super::BrowserPane;

/// `NSModalResponseOK`.
const OK: isize = 1;

/// The path of an `NSURL`.
unsafe fn url_path(url: *mut AnyObject) -> Option<PathBuf> {
    if url.is_null() {
        return None;
    }
    // SAFETY: `url` is a live NSURL (the caller's).
    let path: Option<Retained<NSString>> = unsafe { msg_send![url, path] };
    path.map(|p| PathBuf::from(p.to_string()))
}

/// Show the panel for `dialog`; the chosen paths (`None`: cancelled) arrive on the receiver.
fn show(dialog: &FileDialog) -> tokio::sync::oneshot::Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let save = dialog.mode == FileDialogMode::Save;
    let class = AnyClass::get(if save { c"NSSavePanel" } else { c"NSOpenPanel" });
    // SAFETY: main thread (gpui's UI thread); every object is created,
    // configured and released here or by AppKit with the panel.
    unsafe {
        let Some(class) = class else {
            let _ = tx.send(None);
            return rx;
        };
        let panel: Option<Retained<AnyObject>> = if save { msg_send![class, savePanel] } else { msg_send![class, openPanel] };
        let Some(panel) = panel else {
            let _ = tx.send(None);
            return rx;
        };
        if !save {
            let folder = dialog.mode == FileDialogMode::OpenFolder;
            let _: () = msg_send![&*panel, setCanChooseFiles: Bool::new(!folder)];
            let _: () = msg_send![&*panel, setCanChooseDirectories: Bool::new(folder)];
            let _: () = msg_send![&*panel, setAllowsMultipleSelection: Bool::new(dialog.mode == FileDialogMode::OpenMultiple)];
            let _: () = msg_send![&*panel, setResolvesAliases: Bool::YES];
        }
        let _: () = msg_send![&*panel, setCanCreateDirectories: Bool::YES];
        if let Some(title) = &dialog.title {
            let _: () = msg_send![&*panel, setMessage: &*NSString::from_str(title)];
        }
        if let Some(types) = dialog.accept.as_ref().filter(|_| dialog.mode != FileDialogMode::OpenFolder) {
            let types: Vec<Retained<NSString>> = types.iter().map(|t| NSString::from_str(t)).collect();
            let types = NSArray::from_retained_slice(&types);
            // Deprecated for content types, still honoured, and takes extensions as they are.
            let _: () = msg_send![&*panel, setAllowedFileTypes: &*types];
        }
        if let Some(dir) = dialog.directory() {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
            let _: () = msg_send![&*panel, setDirectoryURL: &*url];
        }
        if save && let Some(name) = dialog.file_name() {
            let _: () = msg_send![&*panel, setNameFieldStringValue: &*NSString::from_str(&name)];
        }
        let tx = Cell::new(Some(tx));
        let chosen = panel.clone();
        let done = block2::RcBlock::new(move |response: isize| {
            let paths = (response == OK).then(|| {
                if save {
                    let url: *mut AnyObject = msg_send![&*chosen, URL];
                    url_path(url).into_iter().collect::<Vec<_>>()
                } else {
                    let urls: *mut AnyObject = msg_send![&*chosen, URLs];
                    let count: usize = if urls.is_null() { 0 } else { msg_send![urls, count] };
                    (0..count).filter_map(|i| url_path(msg_send![urls, objectAtIndex: i])).collect()
                }
            });
            if let Some(tx) = tx.take() {
                let _ = tx.send(paths);
            }
        });
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let key: *mut AnyObject = if app.is_null() { std::ptr::null_mut() } else { msg_send![app, keyWindow] };
        if key.is_null() {
            let _: () = msg_send![&*panel, beginWithCompletionHandler: &*done];
        } else {
            let _: () = msg_send![&*panel, beginSheetModalForWindow: key, completionHandler: &*done];
        }
    }
    rx
}

impl BrowserPane {
    /// `Event::FileDialog`: the panel, then the answer. A page nobody sees
    /// (off screen, in a hidden tab) gets none.
    pub(super) fn file_dialog(&mut self, dialog: FileDialog, cx: &mut gpui::Context<Self>) {
        let Some(page) = self.page.clone() else { return };
        if self.offscreen || self.hidden || self.bounds.is_none() {
            return page.file_dialog_done(dialog.id, None);
        }
        let chosen = show(&dialog);
        cx.spawn(async move |_, _| {
            let paths = chosen.await.ok().flatten();
            page.file_dialog_done(dialog.id, paths.as_deref());
        })
        .detach();
    }
}
