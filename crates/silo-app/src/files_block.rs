//! Files blocks in the workspace: one [`FilesPane`] per Files block shown,
//! the server's events routed to it, and creating new ones (the launcher's
//! Files kind and a typed path, New Files Pane / Tab).

use gpui::{AppContext as _, Context, Window};
use silo_proto::{
    control::{Created, FilesSetParams, Method, SpawnSpec},
    model::{Axis, BlockKind},
};

use crate::{
    files::{FilesPane, FilesPaneEvent},
    workspace::Workspace,
};

pub const ICON: &str = "folder";

/// A typed path split into the folder a Files block opens at and the file
/// it then shows: `/a/b.rs` is (`/a`, `b.rs`) when it is no folder.
#[must_use]
pub fn split_path(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    let (dir, name) = trimmed.rsplit_once('/')?;
    if name.is_empty() || name == "~" {
        return None;
    }
    Some((if dir.is_empty() { "/".to_owned() } else { dir.to_owned() }, name.to_owned()))
}

impl Workspace {
    pub(crate) fn ensure_files(&mut self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.contains_key(block) {
            return;
        }
        let info = self.block_info(block).cloned();
        let (id, rt, control) = (block.to_owned(), self.rt.clone(), self.control());
        let pane = cx.new(|cx| FilesPane::new(id, info, rt, control, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &FilesPaneEvent, _, cx| {
            match event {
                FilesPaneEvent::Activated => {
                    let block = pane.read(cx).block_id.clone();
                    this.pane_activated(block);
                }
                FilesPaneEvent::Header => {}
            }
            cx.notify();
        });
        self.files.insert(block.to_owned(), (pane, sub));
    }

    pub(crate) fn files_focused(&self, window: &Window, cx: &gpui::App) -> Option<String> {
        self.files
            .iter()
            .find(|(_, (p, _))| p.read(cx).focus.contains_focused(window, cx) || p.read(cx).tree_focus.is_focused(window))
            .map(|(id, _)| id.clone())
    }

    /// `files.changed` for one of this window's Files blocks.
    pub(crate) fn files_changed(&mut self, block: &str, paths: &[String], truncated: bool, cx: &mut Context<Self>) {
        if let Some((pane, _)) = self.files.get(block) {
            pane.update(cx, |pane, cx| pane.files_changed(paths, truncated, cx));
        }
    }

    /// New Files Tab: a tab holding a Files block at the focused block's folder.
    pub(crate) fn new_files_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx));
        let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
        self.call(Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: BlockKind::Files, url: None });
    }

    /// New Files Pane: a Files block beside `block`, at its folder.
    pub(crate) fn new_files(&mut self, block: String, cx: &mut Context<Self>) {
        let axis = self.auto_axis(&block);
        let cwd = self.cwd_of(&block, cx);
        self.call(files_split(block, axis, cwd));
    }

    /// Open a Files block for a typed `path` with `method` (its spawn's cwd
    /// is the path): a folder opens there; a file opens at its folder, then
    /// shows. Remote paths are checked by their server, so try the folder
    /// reading first.
    pub(crate) fn open_files_path(&mut self, method: Method, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn(async move {
            let error = match control.call::<Created>(method.clone()).await {
                Ok(_) => return Ok(()),
                Err(error) => error,
            };
            let not_dir = error
                .downcast_ref::<silo_proto::control::RpcError>()
                .is_some_and(|e| e.code == "bad_request" && e.message.contains("not a directory"));
            let path = spawn_cwd(&method).unwrap_or_default();
            let Some((dir, name)) = split_path(&path).filter(|_| not_dir) else { return Err(error) };
            let created: Created = control.call(with_cwd(method, dir)).await?;
            control
                .call_value(Method::FilesSet(FilesSetParams { block_id: created.block_id, active: Some(name), ..FilesSetParams::default() }))
                .await?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            let error = match task.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error,
                Err(error) => anyhow::Error::from(error),
            };
            tracing::warn!("open files: {error:#}");
            let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't open that path"), cx));
        })
        .detach();
    }
}

/// `block.split` for a Files block beside `block` at `cwd`.
pub fn files_split(block: String, axis: Axis, cwd: Option<String>) -> Method {
    let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Files, url: None, agent: None }
}

fn spawn_cwd(method: &Method) -> Option<String> {
    match method {
        Method::BlockSplit { spawn, .. } | Method::WindowCreate { spawn, .. } => spawn.cwd.clone(),
        _ => None,
    }
}

fn with_cwd(method: Method, cwd: String) -> Method {
    match method {
        Method::BlockSplit { block_id, axis, spawn, kind, url, agent } => {
            Method::BlockSplit { block_id, axis, spawn: SpawnSpec { cwd: Some(cwd), ..spawn }, kind, url, agent }
        }
        Method::WindowCreate { session_id, spawn, kind, url } => {
            Method::WindowCreate { session_id, spawn: SpawnSpec { cwd: Some(cwd), ..spawn }, kind, url }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_paths_split_into_folder_and_file() {
        assert_eq!(split_path("/work/app/src/main.rs"), Some(("/work/app/src".into(), "main.rs".into())));
        assert_eq!(split_path("~/notes.md"), Some(("~".into(), "notes.md".into())));
        assert_eq!(split_path("/etc"), Some(("/".into(), "etc".into())));
        assert_eq!(split_path("~"), None);
        assert_eq!(split_path("/"), None);
    }

    #[test]
    fn a_retry_keeps_the_rest_of_the_call() {
        let method = files_split("b1".into(), Axis::Vertical, Some("/a/b.rs".into()));
        assert_eq!(spawn_cwd(&method).as_deref(), Some("/a/b.rs"));
        let Method::BlockSplit { block_id, axis, spawn, kind, .. } = with_cwd(method, "/a".into()) else { panic!() };
        assert_eq!((block_id.as_str(), axis, spawn.cwd.as_deref(), kind), ("b1", Axis::Vertical, Some("/a"), BlockKind::Files));
    }
}
