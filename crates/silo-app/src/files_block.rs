//! Files blocks in the workspace: one [`FilesPane`] per Files block shown,
//! the server's events routed to it, creating new ones (the launcher's
//! Files kind and a typed path, New Files Pane / Tab), and asking before a
//! close loses unsaved edits (a tab, the pane, its tab or its session):
//! Save / Don't Save / Cancel.

use gpui::{AnyElement, AppContext as _, Context, FocusHandle, FontWeight, KeyDownEvent, Window, div, prelude::*, px, rgb};
use silo_proto::{
    control::{Created, FilesSetParams, Method, SpawnSpec},
    model::{Axis, BlockKind},
};

use crate::{
    close_confirm::CloseTarget,
    files::{FilesPane, FilesPaneEvent, model},
    theme::{self, Continuous as _},
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// What goes on once unsaved edits are saved or let go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AfterUnsaved {
    /// Close these tabs of a Files block.
    Tabs { block: String, paths: Vec<String> },
    /// The close, with its busy check.
    Close(CloseTarget),
    /// The close as is (its busy check already done).
    Send { host: usize, target: CloseTarget, force: bool },
}

/// The open Save / Don't Save / Cancel dialog.
pub(crate) struct UnsavedConfirm {
    after: AfterUnsaved,
    /// Each Files block's unsaved tabs.
    files: Vec<(String, Vec<String>)>,
    saving: bool,
    focus: FocusHandle,
}

/// The dialog's question.
pub(crate) fn question(files: &[(String, Vec<String>)]) -> String {
    let paths: Vec<&String> = files.iter().flat_map(|(_, p)| p).collect();
    match paths.as_slice() {
        [one] => format!("Save changes to \u{201c}{}\u{201d}?", model::name(one)),
        many => format!("Save changes to {} files?", many.len()),
    }
}

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
        if self.blocks.files(block).is_some() {
            return;
        }
        let info = self.block_info(block).cloned();
        let (id, rt, control) = (block.to_owned(), self.rt.clone(), self.control());
        let pane = cx.new(|cx| FilesPane::new(id, info, rt, control, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &FilesPaneEvent, window, cx| {
            match event {
                FilesPaneEvent::Activated => {
                    let block = pane.read(cx).block_id.clone();
                    this.pane_activated(block);
                }
                FilesPaneEvent::Header => {}
                FilesPaneEvent::Unsaved(paths) => {
                    let block = pane.read(cx).block_id.clone();
                    let files = vec![(block.clone(), paths.clone())];
                    this.open_unsaved(AfterUnsaved::Tabs { block, paths: paths.clone() }, files, window, cx);
                }
            }
            cx.notify();
        });
        self.blocks.insert(block.to_owned(), crate::blocks::BlockPane::Files(pane), sub);
    }

    /// The unsaved tabs a close of `target` would lose, per Files block.
    pub(crate) fn unsaved_in(&self, target: &CloseTarget, cx: &gpui::App) -> Vec<(String, Vec<String>)> {
        let blocks: Vec<String> = match target {
            CloseTarget::Pane(block) => vec![block.clone()],
            CloseTarget::Tab(window) => self
                .view
                .as_ref()
                .and_then(|v| v.session.windows.iter().find(|w| &w.id == window))
                .map(crate::floats::window_blocks)
                .unwrap_or_default(),
            CloseTarget::Session { id, .. } => self
                .view
                .as_ref()
                .filter(|v| &v.session.id == id)
                .map(|v| v.session.windows.iter().flat_map(crate::floats::window_blocks).collect())
                .unwrap_or_default(),
            CloseTarget::StopServer | CloseTarget::RestartServer => self.blocks.iter().map(|(id, _)| id.clone()).collect(),
        };
        blocks
            .into_iter()
            .filter_map(|b| {
                let unsaved = self.blocks.get(&b)?.unsaved(cx);
                (!unsaved.is_empty()).then_some((b, unsaved))
            })
            .collect()
    }

    /// Ask before `after` when a close of `target` loses edits; false when
    /// nothing is unsaved (go on).
    pub(crate) fn ask_unsaved(&mut self, target: &CloseTarget, after: AfterUnsaved, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let files = self.unsaved_in(target, cx);
        if files.is_empty() {
            return false;
        }
        self.open_unsaved(after, files, window, cx);
        true
    }

    fn open_unsaved(&mut self, after: AfterUnsaved, files: Vec<(String, Vec<String>)>, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let deferred = focus.clone();
        window.defer(cx, move |window, cx| deferred.focus(window, cx));
        self.unsaved = Some(UnsavedConfirm { after, files, saving: false, focus });
        cx.notify();
    }

    fn dismiss_unsaved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unsaved.take().is_some() {
            self.refocus_after_unsaved(window, cx);
            cx.notify();
        }
    }

    fn refocus_after_unsaved(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_window().map(|w| w.focused.clone()) {
            Some(block) => self.focus_block(&block, window, cx),
            None => self.focus.focus(window, cx),
        }
    }

    /// Don't Save, or saved: go on with what was asked.
    fn proceed(&mut self, after: AfterUnsaved, window: &mut Window, cx: &mut Context<Self>) {
        match after {
            AfterUnsaved::Tabs { block, paths } => {
                if let Some(pane) = self.blocks.files(&block) {
                    pane.update(cx, |pane, cx| paths.iter().for_each(|p| pane.close_now(p, cx)));
                }
            }
            AfterUnsaved::Close(target) => self.close_checked(target, window, cx),
            AfterUnsaved::Send { host, target, force } => self.send_close(host, target, force, window, cx),
        }
    }

    fn unsaved_dont_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.unsaved.take().filter(|d| !d.saving) else { return };
        self.refocus_after_unsaved(window, cx);
        self.proceed(dialog.after, window, cx);
    }

    /// Save: every listed tab; when all are saved, go on. A refusal (changed
    /// on disk, read-only) stops here and the tab says why.
    fn unsaved_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.unsaved.as_mut().filter(|d| !d.saving) else { return };
        dialog.saving = true;
        let files = dialog.files.clone();
        let mut waits = Vec::new();
        for (block, paths) in &files {
            if let Some(pane) = self.blocks.files(block) {
                waits.extend(crate::files::save_all(&pane.clone(), paths, cx));
            }
        }
        cx.spawn_in(window, async move |this, cx| {
            let mut ok = true;
            for wait in waits {
                ok &= wait.await.unwrap_or(false);
            }
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(dialog) = this.unsaved.take() else { return };
                this.refocus_after_unsaved(window, cx);
                if ok {
                    this.proceed(dialog.after, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn render_unsaved(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.unsaved.as_ref()?;
        let saving = dialog.saving;
        let button = |id: &'static str, label: &'static str, primary: bool, danger: bool| {
            div()
                .id(id)
                .h(px(28.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .continuous(8.0)
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .when(saving, |el| el.opacity(0.5))
                .when(!saving, |el| el.cursor_pointer())
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
                .when(!primary, |el| {
                    el.bg(theme::control_fill())
                        .text_color(rgb(if danger { theme::danger() } else { theme::foreground() }))
                        .hover(|s| s.bg(theme::row_fill()))
                })
                .child(label)
        };
        let paths: Vec<&String> = dialog.files.iter().flat_map(|(_, p)| p).collect();
        let list = (paths.len() > 1).then(|| {
            div().flex().flex_col().gap(px(2.0)).children(
                paths
                    .iter()
                    .take(8)
                    .map(|p| div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).child((*p).clone())),
            )
        });
        let card = div()
            .id("unsaved-confirm")
            .track_focus(&dialog.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                match k.key.as_str() {
                    "enter" => this.unsaved_save(window, cx),
                    "backspace" | "d" if k.modifiers.platform => this.unsaved_dont_save(window, cx),
                    "escape" => this.dismiss_unsaved(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .occlude()
            .w(px(400.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(18.0))
            .continuous(14.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(14.0)).child(question(&dialog.files)))
            .children(list)
            .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(if saving {
                "Saving…"
            } else {
                "Your changes are lost if you don't save them."
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        button("unsaved-dont-save", "Don't Save", false, true)
                            .tooltip(crate::tooltip::Tooltip::text("⌘⌫"))
                            .on_click(cx.listener(|this, _, window, cx| this.unsaved_dont_save(window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        button("unsaved-cancel", "Cancel", false, false)
                            .tooltip(crate::tooltip::Tooltip::text("esc"))
                            .on_click(cx.listener(|this, _, window, cx| this.dismiss_unsaved(window, cx))),
                    )
                    .child(
                        button("unsaved-save", "Save", true, false)
                            .tooltip(crate::tooltip::Tooltip::text("↩"))
                            .on_click(cx.listener(|this, _, window, cx| this.unsaved_save(window, cx))),
                    ),
            );
        Some(
            div()
                .id("unsaved-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(TITLEBAR_HEIGHT + 60.0))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.dismiss_unsaved(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// The view's Files panes are going (another session or host shown):
    /// their unsaved tabs wait for the blocks' next panes.
    pub(crate) fn stash_files(&self, cx: &mut Context<Self>) {
        for (_, pane) in self.blocks.files_panes() {
            pane.update(cx, |pane, cx| pane.stash(cx));
        }
    }

    /// `files.changed` for one of this window's Files blocks.
    pub(crate) fn files_changed(&mut self, block: &str, paths: &[String], truncated: bool, cx: &mut Context<Self>) {
        if let Some(pane) = self.blocks.files(block) {
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
    fn the_question_names_one_file_or_counts_them() {
        assert_eq!(question(&[("b".into(), vec!["src/main.rs".into()])]), "Save changes to \u{201c}main.rs\u{201d}?");
        assert_eq!(question(&[("b".into(), vec!["a".into(), "b".into()]), ("c".into(), vec!["d".into()])]), "Save changes to 3 files?");
    }

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
