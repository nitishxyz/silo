//! New files and folders from the tree (a name bar over the viewer, in the
//! selected row's folder), and Open Folder… (`files.root`): the native
//! picker for this Mac's files, a path bar for another host's.

use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use silo_proto::control::{FileWritten, FilesCreateParams, FilesDone, FilesMkdirParams, FilesRootParams, FilesSetParams, Method};
use silo_proto::model::FilesState;

use super::{FilesPane, describe, model};
use crate::text_field::{FieldEvent, TextField};

/// What the name bar asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    File { dir: String },
    Folder { dir: String },
    Root,
}

pub(crate) struct NameBar {
    pub ask: Ask,
    pub field: Entity<TextField>,
    _sub: Subscription,
}

/// The folder new items go in: the selected folder, the selected file's
/// folder, else the root.
#[must_use]
pub fn target_folder(selected: Option<(&str, bool)>) -> String {
    match selected {
        Some((path, true)) => path.to_owned(),
        Some((path, false)) => model::parent(path).to_owned(),
        None => String::new(),
    }
}

/// A typed name (or `a/b.rs`) in `dir`: none for empty names, absolute
/// paths and `.` / `..` parts.
#[must_use]
pub fn new_path(dir: &str, typed: &str) -> Option<String> {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty() || typed.starts_with('/') || typed.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return None;
    }
    Some(model::join(dir, typed))
}

impl Ask {
    /// The bar's label: the folder's name, never its path.
    pub(crate) fn label(&self, root: &str) -> String {
        let name = |dir: &str| if dir.is_empty() { model::name(root).to_owned() } else { model::name(dir).to_owned() };
        match self {
            Self::File { dir } => format!("New file in {}", name(dir)),
            Self::Folder { dir } => format!("New folder in {}", name(dir)),
            Self::Root => "Open folder".into(),
        }
    }
}

impl FilesPane {
    /// The folder new items from the tree go in.
    fn new_item_dir(&self) -> String {
        let selected = self.tree_selected.as_deref().map(|p| (p, self.tree_rows().iter().any(|r| r.path == p && r.dir)));
        target_folder(selected)
    }

    pub(crate) fn ask_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.new_item_dir();
        self.open_name_bar(Ask::File { dir }, "name.ext", window, cx);
    }

    pub(crate) fn ask_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.new_item_dir();
        self.open_name_bar(Ask::Folder { dir }, "Name", window, cx);
    }

    /// Open Folder…: this Mac's picker, or a path bar for another host.
    pub(crate) fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.local {
            return self.open_name_bar(Ask::Root, "A path, ~/… or a folder in this one", window, cx);
        }
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(dir) = paths.into_iter().next().and_then(|p| p.to_str().map(str::to_owned)) else { return };
            let _ = this.update(cx, |this, cx| this.reroot(dir, cx));
        })
        .detach();
    }

    fn open_name_bar(&mut self, ask: Ask, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.goto = None;
        let field = cx.new(|cx| TextField::new(placeholder, cx));
        let sub = cx.subscribe_in(&field, window, |this, field, event: &FieldEvent, window, cx| match event {
            FieldEvent::Submit => {
                let text = field.read(cx).text.clone();
                this.name_submit(&text, window, cx);
            }
            FieldEvent::Cancel => this.close_bars(window, cx),
            _ => {}
        });
        let focus = field.read(cx).focus.clone();
        self.name_bar = Some(NameBar { ask, field, _sub: sub });
        focus.focus(window, cx);
        cx.notify();
    }

    fn name_submit(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.name_bar.as_ref() else { return };
        let ask = bar.ask.clone();
        if ask == Ask::Root {
            let path = text.trim().to_owned();
            if !path.is_empty() {
                self.close_bars(window, cx);
                self.reroot(path, cx);
            }
            return;
        }
        let (Ask::File { dir } | Ask::Folder { dir }) = &ask else { return };
        let Some(path) = new_path(dir, text) else { return };
        self.close_bars(window, cx);
        let parent = model::parent(&path).to_owned();
        if let Ask::Folder { .. } = ask {
            let method = Method::FilesMkdir(FilesMkdirParams { block_id: self.block_id.clone(), path, unique: false });
            self.request(method, cx, move |this, result: anyhow::Result<FilesDone>, cx| match result {
                Ok(done) if done.failed.is_empty() => {
                    this.sound(crate::sound::Sound::Open);
                    this.show_created(&parent, done.done.first().map(|m| m.to.clone()), cx);
                }
                Ok(done) => this.created_failed("folder", done.failed.first().map_or_else(String::new, |f| f.message.clone()), cx),
                Err(error) => this.created_failed("folder", describe(&error), cx),
            });
        } else {
            let method = Method::FilesCreate(FilesCreateParams { block_id: self.block_id.clone(), path, text: String::new() });
            self.request(method, cx, move |this, result: anyhow::Result<FileWritten>, cx| match result {
                Ok(written) => {
                    this.show_created(&parent, None, cx);
                    this.open(&written.path, true, cx);
                }
                Err(error) => this.created_failed("file", describe(&error), cx),
            });
        }
    }

    /// A new item in `parent`: its folder listed and open in the tree, a
    /// new folder selected.
    fn show_created(&mut self, parent: &str, folder: Option<String>, cx: &mut Context<Self>) {
        let mut expanded = self.state.expanded.clone();
        let mut dir = parent;
        while !dir.is_empty() {
            if !expanded.iter().any(|e| e == dir) {
                expanded.push(dir.to_owned());
            }
            dir = model::parent(dir);
        }
        if expanded != self.state.expanded {
            self.state.expanded.clone_from(&expanded);
            self.set(FilesSetParams { expanded: Some(expanded), ..FilesSetParams::default() }, cx);
        }
        self.list(parent.to_owned(), cx);
        if let Some(folder) = folder {
            self.tree_selected = Some(folder.clone());
            self.tree_reveal = Some(folder);
        }
        cx.notify();
    }

    fn created_failed(&mut self, what: &str, why: String, cx: &mut Context<Self>) {
        self.notice = Some(format!("Couldn't create the {what}: {why}"));
        cx.notify();
    }

    /// Re-root the block at `path` (absolute, `~/…`, or under the root).
    pub(crate) fn reroot(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.unsaved().is_empty() {
            self.notice = Some("Save or close the edited tabs first".into());
            return cx.notify();
        }
        let method = Method::FilesRoot(FilesRootParams { block_id: self.block_id.clone(), path });
        self.request(method, cx, |this, result: anyhow::Result<FilesState>, cx| match result {
            Ok(state) => this.adopt(state, cx),
            Err(error) => {
                this.notice = Some(format!("Couldn't open that folder: {}", describe(&error)));
                cx.notify();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_items_go_in_the_selected_folder() {
        assert_eq!(target_folder(Some(("src/files", true))), "src/files");
        assert_eq!(target_folder(Some(("src/files/mod.rs", false))), "src/files");
        assert_eq!(target_folder(Some(("README.md", false))), "");
        assert_eq!(target_folder(None), "");
    }

    #[test]
    fn typed_names_stay_in_their_folder() {
        assert_eq!(new_path("src", " main.rs ").as_deref(), Some("src/main.rs"));
        assert_eq!(new_path("", "docs/new.md").as_deref(), Some("docs/new.md"));
        assert_eq!(new_path("src", "util/").as_deref(), Some("src/util"));
        for bad in ["", "  ", "/etc/x", "../x", "a/../b", "a//b", "./a"] {
            assert_eq!(new_path("src", bad), None, "{bad}");
        }
        assert_eq!(Ask::File { dir: String::new() }.label("/work/app"), "New file in app");
        assert_eq!(Ask::Folder { dir: "src/files".into() }.label("/work/app"), "New folder in files");
    }
}
