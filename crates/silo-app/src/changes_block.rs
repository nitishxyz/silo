//! Changes blocks in the workspace (docs/CHANGES_BLOCK.md §3, §5): one
//! [`ChangesPane`] per Changes block shown, `changes.changed` routed to
//! it, creating new ones (New Changes Tab / Pane, an agent's Review), the
//! revert confirm (the close guard's card), and the review loop's trips to
//! otto: Send review queues the comments on the linked agent, Ask agent
//! queues the commit message ask and fills the footer with the reply.

use std::time::Duration;

use gpui::{AnyElement, AppContext as _, Context, FocusHandle, KeyDownEvent, Window, div, prelude::*, px};
use otto_proto::{Delivery, InputPart, SessionStatus};
use silo_proto::{
    control::{Created, FilesSetParams, Method, SpawnSpec},
    model::{AgentLink, Axis, BlockKind},
};

use crate::{
    changes::{ChangesPane, ChangesPaneEvent, RevertAsk, model},
    workspace::Workspace,
};

pub const ICON: &str = "git-diff";

/// How long Ask agent waits for the reply.
const ASK_WAIT: Duration = Duration::from_secs(600);
const ASK_POLL: Duration = Duration::from_millis(1500);

/// The open "Discard …?" card.
pub(crate) struct RevertConfirm {
    ask: RevertAsk,
    focus: FocusHandle,
}

/// The turns the scope picker lists: those with both snapshots.
#[must_use]
pub(crate) fn turn_refs(turns: &[otto_proto::TurnSummary]) -> Vec<model::TurnRef> {
    turns
        .iter()
        .filter_map(|t| {
            Some(model::TurnRef {
                turn: t.turn,
                base_tree: t.changes.base_tree.clone()?,
                end_tree: t.changes.end_tree.clone()?,
                files: t.changes.file_count(),
            })
        })
        .collect()
}

/// `block.split` for a Changes block beside `block` at `cwd`.
pub fn changes_split(block: String, axis: Axis, cwd: Option<String>) -> Method {
    let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Changes, url: None, agent: None }
}

impl Workspace {
    pub(crate) fn ensure_changes(&mut self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocks.changes(block).is_some() {
            return;
        }
        let info = self.block_info(block).cloned();
        let (id, rt, control) = (block.to_owned(), self.rt.clone(), self.control());
        let pane = cx.new(|cx| ChangesPane::new(id, info, rt, control, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &ChangesPaneEvent, window, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                ChangesPaneEvent::Activated => this.pane_activated(block),
                ChangesPaneEvent::Header => {}
                ChangesPaneEvent::Revert(ask) => this.open_revert(ask.clone(), window, cx),
                ChangesPaneEvent::SendReview { text, link } => this.send_review(block, text.clone(), link.clone(), cx),
                ChangesPaneEvent::AskCommit { link } => this.ask_commit(block, link.clone(), cx),
                ChangesPaneEvent::OpenFile { root, path, line } => this.open_change_in_files(block, root.clone(), path.clone(), *line, cx),
                ChangesPaneEvent::Turns(session) => this.load_turns(block, session.clone(), cx),
            }
            cx.notify();
        });
        self.blocks.insert(block.to_owned(), crate::blocks::BlockPane::Changes(pane), sub);
    }

    /// `changes.changed` for one of this window's Changes blocks.
    pub(crate) fn changes_changed(&mut self, block: &str, cx: &mut Context<Self>) {
        if let Some(pane) = self.blocks.changes(block) {
            pane.update(cx, |pane, cx| pane.changes_changed(cx));
        }
    }

    /// New Changes Tab: a tab reviewing the focused block's repo.
    pub(crate) fn new_changes_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let cwd = self.new_block_folder(window, cx);
        let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
        self.call(Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: BlockKind::Changes, url: None });
    }

    /// New Changes Pane: a Changes block beside `block`, on its repo.
    pub(crate) fn new_changes(&mut self, block: String, cx: &mut Context<Self>) {
        let axis = self.auto_axis(&block);
        let cwd = self.cwd_of(&block, cx);
        self.call(changes_split(block, axis, cwd));
    }

    /// ⌘P Review Changes: this session's Changes block, else a new one
    /// beside `block` (a tab with none focused).
    pub(crate) fn review_changes_command(&mut self, block: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.kind == BlockKind::Changes).map(|b| b.id.clone()));
        match (existing, block) {
            (Some(changes), _) => self.reveal_agent_target(changes, window, cx),
            (None, Some(block)) => self.new_changes(block, cx),
            (None, None) => self.new_changes_tab(window, cx),
        }
    }

    /// The agent block this session holds that a review without a link goes to:
    /// the one in `block`'s tab, else the session's first.
    fn agent_for(&self, block: &str) -> Option<AgentLink> {
        let view = self.view.as_ref()?;
        let tab = view.session.windows.iter().find(|w| crate::floats::window_blocks(w).iter().any(|b| b == block));
        let agents: Vec<_> =
            view.blocks.iter().filter(|b| b.kind == BlockKind::Agent && b.agent.as_ref().is_some_and(|a| a.session_id.is_some())).collect();
        let in_tab = tab.map(crate::floats::window_blocks).unwrap_or_default();
        let pick = agents.iter().find(|b| in_tab.contains(&b.id)).or_else(|| agents.first())?;
        Some(AgentLink { otto_session: pick.agent.as_ref()?.session_id.clone()?, block_id: Some(pick.id.clone()) })
    }

    /// Queue `text` on `session` through this host's otto.
    fn queue_on(&self, session: String, text: String) -> Option<tokio::task::JoinHandle<Result<String, String>>> {
        let target = self.agent_target(self.active_host)?;
        Some(self.rt.spawn(async move {
            let c = crate::agent::client::connection(&target).await.map_err(|e| crate::agent::describe(&e))?;
            c.prompt(&session, vec![InputPart::Text { text }], Delivery::Queue).await.map_err(|e| crate::agent::describe(&e))?;
            Ok(session)
        }))
    }

    /// Send review: the comments as one message, queued on the agent.
    fn send_review(&mut self, block: String, text: String, link: Option<AgentLink>, cx: &mut Context<Self>) {
        let Some(link) = link.or_else(|| self.agent_for(&block)) else {
            return self.review_done(&block, Err("No agent to send it to: open Review from an agent's turn".into()), cx);
        };
        let Some(task) = self.queue_on(link.otto_session, text) else {
            return self.review_done(&block, Err(self.agents_not_offered().unwrap_or_else(|| "Agents aren't available here".into())), cx);
        };
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|e| e.to_string()).and_then(|r| r).map(|_| ());
            let _ = this.update(cx, |this, cx| this.review_done(&block, result, cx));
        })
        .detach();
    }

    fn review_done(&mut self, block: &str, result: Result<(), String>, cx: &mut Context<Self>) {
        if let Some(pane) = self.blocks.changes(block) {
            pane.update(cx, |pane, cx| pane.review_sent(result, cx));
        }
    }

    /// Ask agent: queue the ask, wait for the turn to end, hand its reply to the footer.
    fn ask_commit(&mut self, block: String, link: Option<AgentLink>, cx: &mut Context<Self>) {
        let link = link.or_else(|| self.agent_for(&block));
        let done = move |this: &mut Self, result: Result<String, String>, cx: &mut Context<Self>| {
            if let Some(pane) = this.blocks.changes(&block) {
                pane.update(cx, |pane, cx| pane.commit_message(result, cx));
            }
        };
        let Some(link) = link else {
            return done(self, Err("No agent to ask: open Review from an agent's turn".into()), cx);
        };
        let Some(target) = self.agent_target(self.active_host) else {
            return done(self, Err("Agents aren't available here".into()), cx);
        };
        let session = link.otto_session;
        let task = self.rt.spawn(async move {
            let describe = |e: otto_client::ClientError| crate::agent::describe(&e);
            let c = crate::agent::client::connection(&target).await.map_err(describe)?;
            let before: std::collections::HashSet<String> =
                c.session_view(&session).await.map_err(describe)?.items.into_iter().map(|i| i.id).collect();
            c.prompt(&session, vec![InputPart::Text { text: model::COMMIT_ASK.to_owned() }], Delivery::Queue).await.map_err(describe)?;
            let start = std::time::Instant::now();
            let mut asked = false;
            while start.elapsed() < ASK_WAIT {
                tokio::time::sleep(ASK_POLL).await;
                let view = c.session_view(&session).await.map_err(describe)?;
                let new: Vec<&otto_llm::Item> = view.items.iter().filter(|i| !before.contains(&i.id)).collect();
                // The ask itself is in once otto took it from the queue.
                asked |= new.iter().any(|i| i.role == otto_llm::Role::User && i.text().contains(model::COMMIT_ASK));
                let idle = matches!(view.info.status, SessionStatus::Idle | SessionStatus::Error);
                if asked && idle {
                    let reply = new.iter().rev().find(|i| i.role == otto_llm::Role::Assistant && !i.text().trim().is_empty());
                    return reply.map(|i| i.text()).ok_or_else(|| "The agent didn't reply".to_owned());
                }
            }
            Err("The agent hasn't replied yet".to_owned())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|e| e.to_string()).and_then(|r| r);
            let _ = this.update(cx, |this, cx| done(this, result, cx));
        })
        .detach();
    }

    /// The linked agent's turns, for `block`'s scope picker.
    fn load_turns(&mut self, block: String, session: String, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target(self.active_host) else { return };
        let id = session.clone();
        let task = self.rt.spawn(async move {
            let c = crate::agent::client::connection(&target).await.ok()?;
            c.session_view(&id).await.ok().map(|v| v.turns)
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some(turns)) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(pane) = this.blocks.changes(&block) {
                    pane.update(cx, |pane, cx| pane.set_turns(&session, turn_refs(&turns), cx));
                }
            });
        })
        .detach();
    }

    /// ⌘-click in a Changes block: the file in a Files block beside it, scrolled to `line`.
    fn open_change_in_files(&mut self, block: String, root: String, path: String, line: u32, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let split = crate::files_block::files_split(block.clone(), self.auto_axis(&block), Some(root));
        let task = self.rt.spawn(async move {
            let created: Created = control.call(split).await?;
            let scroll = std::collections::BTreeMap::from([(path.clone(), u64::from(line.saturating_sub(6)))]);
            control
                .call_value(Method::FilesSet(FilesSetParams {
                    block_id: created.block_id,
                    active: Some(path),
                    scroll: Some(scroll),
                    ..FilesSetParams::default()
                }))
                .await?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) | Err(error) = task.await.map_err(anyhow::Error::from) {
                let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't open the file"), cx));
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ revert confirm

    fn open_revert(&mut self, ask: RevertAsk, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let deferred = focus.clone();
        window.defer(cx, move |window, cx| deferred.focus(window, cx));
        self.revert = Some(RevertConfirm { ask, focus });
        cx.notify();
    }

    fn dismiss_revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.revert.take() {
            self.focus_block(&dialog.ask.block, window, cx);
            cx.notify();
        }
    }

    fn confirm_revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.revert.take() else { return };
        let RevertAsk { block, path, hunks, .. } = dialog.ask;
        self.focus_block(&block, window, cx);
        if let Some(pane) = self.blocks.changes(&block) {
            pane.update(cx, |pane, cx| pane.revert_now(&path, hunks, cx));
        }
        cx.notify();
    }

    pub(crate) fn render_revert(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.revert.as_ref()?;
        let card = crate::confirm_card::card("revert-confirm")
            .track_focus(&dialog.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                match k.key.as_str() {
                    "backspace" if k.modifiers.platform => this.confirm_revert(window, cx),
                    "escape" | "enter" => this.dismiss_revert(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(crate::confirm_card::headline(dialog.ask.question.clone()))
            .child(crate::confirm_card::detail(dialog.ask.detail.clone()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        crate::confirm_card::button("revert-confirm-discard", "Discard", false, true)
                            .cursor_pointer()
                            .tooltip(crate::tooltip::Tooltip::text("⌘⌫"))
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_revert(window, cx))),
                    )
                    .child(
                        crate::confirm_card::button("revert-confirm-cancel", "Cancel", true, false)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| this.dismiss_revert(window, cx))),
                    ),
            );
        Some(crate::confirm_card::backdrop("revert-confirm-backdrop", card, cx.listener(|this, _, window, cx| this.dismiss_revert(window, cx))))
    }
}

#[cfg(test)]
mod tests {
    use otto_proto::{TurnChanges, TurnSummary};

    use super::*;

    fn turn(turn: i64, trees: Option<(&str, &str)>) -> TurnSummary {
        let changes = TurnChanges {
            base_tree: trees.map(|t| t.0.into()),
            end_tree: trees.map(|t| t.1.into()),
            files: vec!["a.rs".into(), "b.rs".into()],
            more: 1,
            added: 3,
            removed: 1,
        };
        TurnSummary { turn, item_id: None, changes }
    }

    #[test]
    fn the_picker_lists_snapshotted_turns() {
        let turns = [turn(2, Some(("b2", "e2"))), turn(3, None), turn(5, Some(("b5", "e5")))];
        let refs = turn_refs(&turns);
        assert_eq!(refs.iter().map(|t| (t.turn, t.files)).collect::<Vec<_>>(), [(2, 3), (5, 3)]);
    }
}
