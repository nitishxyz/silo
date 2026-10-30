//! Move a session to another folder from the switcher (⌘O or a row's `folder` button): the recent projects, then
//! `Choose folder…` (this Mac's picker, or a typed path for another host's otto, which resolves it there).
//! otto moves the idle session (`session.update {folder}`) and its next turn works there; a block showing it follows.

use super::{AgentPane, AgentPaneEvent, Overlay};
use gpui::{Context, Window};
use otto_proto::{ProjectInfo, SessionInfo};

/// The session being moved while the recent projects show.
pub(super) struct Relocate {
    pub session: String,
    /// Another host's folder: the filter takes its path.
    pub typing: bool,
}

impl AgentPane {
    /// The switcher's ⌘O: the highlighted session, else the shown one.
    pub(super) fn move_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let highlighted = self.selected.checked_sub(1).and_then(|i| self.session_choices(cx).get(i).map(|s| s.id.clone()));
        if let Some(session) = highlighted.or_else(|| self.session_id.clone()) {
            self.start_move(session, window, cx);
        }
    }

    /// The recent projects, picking a folder for `session`.
    pub(super) fn start_move(&mut self, session: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(Overlay::Projects) {
            self.set_overlay(None, cx);
        }
        self.open_overlay(Overlay::Projects, window, cx);
        self.relocate = Some(Relocate { session, typing: false });
        cx.notify();
    }

    /// A pick in the recent projects while moving: a project, or `Choose folder…` (`index` past them).
    pub(super) fn pick_folder(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Relocate { session, typing }) = &self.relocate else { return };
        let session = session.clone();
        if *typing {
            return self.submit_path(window, cx);
        }
        if let Some(root) = self.project_choices(cx).get(index).map(|p| p.root.clone()) {
            return self.move_session(session, root, window, cx);
        }
        if self.host.is_some() {
            self.relocate = Some(Relocate { session, typing: true });
            self.selected = 0;
            self.filter.update(cx, |f, cx| {
                f.replace_text("", cx);
                f.set_placeholder("A path or ~/…", cx);
            });
            return cx.notify();
        }
        self.close_overlay(window, cx);
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Move".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(dir) = paths.into_iter().next().and_then(|p| p.to_str().map(str::to_owned)) else { return };
            let _ = this.update_in(cx, |this, window, cx| this.move_session(session, dir, window, cx));
        })
        .detach();
    }

    /// ⏎ on a typed path.
    pub(super) fn submit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.filter.read(cx).text.trim().to_owned();
        let Some(session) = self.relocate.as_ref().map(|r| r.session.clone()).filter(|_| !path.is_empty()) else { return };
        self.move_session(session, path, window, cx);
    }

    fn move_session(&mut self, session: String, folder: String, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlay(window, cx);
        let update = otto_client::SessionUpdate { folder: Some(folder), ..Default::default() };
        self.run(
            cx,
            move |c| async move { c.session_update(&session, update).await },
            |this, result, cx| match result {
                Ok(info) => this.moved(&info, cx),
                Err(error) => this.notice = Some(moved_error(&error)),
            },
        );
    }

    /// otto moved `info`'s session: a block showing it goes along (one `agent.set`, so it keeps the session);
    /// else it leaves this project's list.
    fn moved(&mut self, info: &SessionInfo, cx: &mut Context<Self>) {
        if self.session_id.as_deref() != Some(info.id.as_str()) || self.cwd.as_deref() == Some(info.cwd.as_str()) {
            return self.refresh_sessions(cx);
        }
        self.cwd = Some(info.cwd.clone());
        self.moving = Some(info.cwd.clone());
        self.project =
            Some(ProjectInfo { id: info.project_id.clone(), root: info.cwd.clone(), created_at: info.created_at, updated_at: info.updated_at });
        self.sessions = vec![info.clone()];
        self.load(cx);
        let title = self.header_title().unwrap_or_default();
        cx.emit(AgentPaneEvent::Moved { cwd: info.cwd.clone(), session_id: info.id.clone(), title });
        cx.notify();
    }
}

/// otto's refusal in the notice slot, without paths.
fn moved_error(error: &str) -> String {
    if error.contains("busy") {
        "Wait for the turn to end to move the session".into()
    } else if error.contains("absolute") {
        "Type a full path or ~/…".into()
    } else if error.contains("not a folder") || error.contains("No such file") {
        "That folder doesn't exist".into()
    } else {
        format!("Couldn't move the session: {error}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refusals_read_without_paths() {
        assert_eq!(super::moved_error("cannot move a busy session; wait for its turn to end"), "Wait for the turn to end to move the session");
        assert_eq!(super::moved_error("folder must be absolute or ~/…"), "Type a full path or ~/…");
        assert_eq!(super::moved_error("No such file or directory (os error 2)"), "That folder doesn't exist");
    }
}
