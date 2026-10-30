//! Opening, driving and closing the launcher, and creating what it picks.

use std::time::Duration;

use gpui::{AppContext as _, Context, Window};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::BlockKind,
};

use super::{Dialog, Kind, Row, Target, chooser_target, digit_pick, intent_note, preselect, rows, same_kind, side_key, split_method, step};
use crate::{
    commands::Command,
    text_field::{FieldEvent, TextField},
    workspace::Workspace,
};

impl Workspace {
    /// The kind ⌘T opens for the focused block of the active window.
    fn same_kind_now(&self, window: &Window, cx: &gpui::App) -> Kind {
        let focused = self.focused_block(window, cx);
        same_kind(focused.as_deref(), self.view.as_ref().map_or(&[], |v| v.blocks.as_slice()))
    }

    /// ⌘T: a tab with the focused block's kind, gated like its own command.
    pub(crate) fn new_tab_like_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.same_kind_now(window, cx);
        self.run(kind.tab_command(), window, cx);
    }

    pub(crate) fn close_launcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launcher.dialog.take().is_some() {
            self.refocus_block(window, cx);
            cx.notify();
        }
    }

    /// The recent pages of the shown host, newest first.
    fn launcher_recents(&self, cx: &gpui::App) -> Vec<crate::browser_home::RecentPage> {
        let Some(host) = self.hosts.get(self.active_host) else { return Vec::new() };
        let key = if host.info.local { "local".to_owned() } else { host.info.transport.remote_id().unwrap_or_default() };
        crate::browser_home::listed(&crate::hosts_state::HostsState::global(cx).read(cx).app_state().recent_pages, &key)
    }

    /// Open the launcher `chooser` stands for (New Tab… or New Pane…), or
    /// close it if that one is open.
    pub(crate) fn toggle_launcher(&mut self, chooser: Command, window: &mut Window, cx: &mut Context<Self>) {
        let block = if chooser == Command::NewPaneChooser { self.focused_block(window, cx) } else { None };
        let Some(target) = chooser_target(chooser, block, |b| self.auto_axis(b)) else { return };
        let same_target = |t: &Target| matches!((t, &target), (Target::Tab, Target::Tab) | (Target::Split { .. }, Target::Split { .. }));
        if self.launcher.dialog.as_ref().is_some_and(|d| same_target(&d.target)) {
            return self.close_launcher(window, cx);
        }
        // Splits open what ⌘D does (a terminal); tabs what ⌘T does.
        let same = if target == Target::Tab { self.same_kind_now(window, cx) } else { Kind::Terminal };
        // One dialog at a time.
        self.close_switcher(window, cx);
        crate::sound::play(crate::sound::Sound::Pop);
        let placeholder =
            if target == Target::Tab { "New tab: kind, address, path, >command, @agent" } else { "New pane: kind, address, path, >command, @agent" };
        let field = cx.new(|cx| TextField::new(placeholder, cx).on_glass());
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.launcher_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        let recents = self.launcher_recents(cx);
        let rows = self.launcher_rows("", &recents);
        let selected = preselect("", &rows, same);
        self.launcher.dialog = Some(Dialog { field, rows, selected, target, same, recents, note: None, _sub: sub });
        // Deferred: a mouse-down that opened us may still focus its target.
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    /// A kind the host has no blocks of isn't offered (Changes on an older server).
    fn launcher_hidden(&self) -> Option<Kind> {
        (!self.host_supports(silo_proto::compat::feature::CHANGES)).then_some(Kind::Changes)
    }

    fn launcher_rows(&self, text: &str, recents: &[crate::browser_home::RecentPage]) -> Vec<Row> {
        let hidden = self.launcher_hidden();
        rows(text, recents).into_iter().filter(|r| hidden.is_none_or(|h| r.kind() != h)).collect()
    }

    fn launcher_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let hidden = self.launcher_hidden();
        let Some(dialog) = self.launcher.dialog.as_mut() else { return };
        let count = dialog.rows.len();
        match event {
            FieldEvent::Changed(text) => {
                dialog.rows = rows(text, &dialog.recents).into_iter().filter(|r| hidden.is_none_or(|h| r.kind() != h)).collect();
                dialog.selected = preselect(text, &dialog.rows, dialog.same);
                dialog.note = intent_note(text);
            }
            FieldEvent::Up => dialog.selected = step(dialog.selected, count, -1),
            FieldEvent::Down => dialog.selected = step(dialog.selected, count, 1),
            FieldEvent::Cancel => return self.close_launcher(window, cx),
            FieldEvent::Submit => {
                let index = dialog.selected;
                return self.launcher_choose(index, window, cx);
            }
        }
        cx.notify();
    }

    /// Tab, ⇧tab, ← or → in a New Pane… dialog moves its side (the ghost
    /// follows); true if it did. A tab's dialog leaves them to the field.
    pub(super) fn launcher_side_key(&mut self, key: &str, shift: bool, cx: &mut Context<Self>) -> bool {
        let Some(delta) = side_key(key, shift) else { return false };
        if !self.launcher.step_side(delta) {
            return false;
        }
        cx.notify();
        true
    }

    /// A digit typed into the empty field picks its row; true if it did.
    pub(super) fn launcher_digit(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(dialog) = self.launcher.dialog.as_ref() else { return false };
        let Some(index) = digit_pick(key, &dialog.field.read(cx).text, dialog.rows.len()) else { return false };
        self.launcher_choose(index, window, cx);
        true
    }

    /// Whether the host can create `kind` (agents need otto on the host).
    pub(super) fn launcher_supported(&self, kind: Kind, target: &Target) -> bool {
        let command = if *target == Target::Tab { kind.tab_command() } else { kind.split_command() };
        command.feature().is_none_or(|f| self.host_supports(f)) && (kind != Kind::Agent || self.agents_offered(self.active_host))
    }

    /// What an unsupported row says.
    pub(super) fn launcher_unsupported_note(&self, kind: Kind) -> String {
        if kind == Kind::Agent
            && let Some(note) = self.agents_not_offered()
        {
            return note;
        }
        self.not_supported()
    }
}

impl Workspace {
    /// Create row `index`; a row the host can't create stays, saying so.
    pub(super) fn launcher_choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.launcher.dialog.as_ref() else { return };
        let Some(row) = dialog.rows.get(index).cloned() else { return };
        let target = dialog.target.clone();
        if !self.launcher_supported(row.kind(), &target) {
            let note = self.launcher_unsupported_note(row.kind());
            if let Some(dialog) = self.launcher.dialog.as_mut() {
                dialog.note = Some(note);
            }
            return cx.notify();
        }
        self.close_launcher(window, cx);
        self.launch(row, target, window, cx);
    }

    /// Create `row` at `target` with the calls the instant commands make.
    fn launch(&mut self, row: Row, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match (target, row) {
            (Target::Tab, Row::Kind(kind, _)) => self.run(kind.tab_command(), window, cx),
            (Target::Tab, Row::Open { url, .. }) => self.open_page_tab(url),
            (Target::Tab, Row::Path(path)) => {
                let Some(view) = &self.view else { return };
                let spawn = SpawnSpec { cwd: Some(path), command: Vec::new(), size: None };
                let method =
                    Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: BlockKind::Files, url: None, background: false };
                self.open_files_path(method, cx);
            }
            (Target::Tab, Row::Run(command)) => {
                let Some(view) = &self.view else { return };
                let cwd = self.new_block_folder(window, cx);
                let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
                let method =
                    Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: BlockKind::Terminal, url: None, background: false };
                self.launch_command(method, command, cx);
            }
            (Target::Split { block, side, .. }, row) => {
                let method = split_method(&block, side, &row, self.cwd_of(&block, cx));
                match row {
                    Row::Kind(Kind::Terminal, _) => self.split_block(block, side.axis(), cx),
                    Row::Kind(Kind::Browser, _) => self.create_browser(method, window, cx),
                    Row::Kind(Kind::Agent, _) => self.create_agent(method, cx),
                    Row::Kind(Kind::Files | Kind::Changes, _) | Row::Open { .. } => self.call(method),
                    Row::Path(_) => self.open_files_path(method, cx),
                    Row::Run(command) => self.launch_command(method, command, cx),
                }
            }
        }
    }

    /// Create a terminal with `method`, then type `command` into its shell
    /// and press return, as the agent's `terminal_open` does: the shell
    /// stays once the command ends.
    fn launch_command(&mut self, method: Method, command: String, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn(async move {
            let created: Created = control.call(method).await?;
            // The shell starts reading once it is up; input typed before is kept.
            tokio::time::sleep(Duration::from_millis(300)).await;
            control.call_value(Method::TerminalSend { block_id: created.block_id.clone(), text: command }).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            control.call_value(Method::TerminalSend { block_id: created.block_id, text: "\r".into() }).await?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            let error = match task.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error,
                Err(error) => anyhow::Error::from(error),
            };
            tracing::warn!("launch command: {error:#}");
            let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't run that command"), cx));
        })
        .detach();
    }
}
