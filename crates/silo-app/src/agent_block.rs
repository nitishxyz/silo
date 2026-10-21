//! Workspace wiring of agent blocks: an [`AgentPane`] per agent block of
//! the local host, its events sent to the server (`docs/AGENT_BLOCK.md`).

use gpui::AppContext as _;
use silo_proto::{
    agent::BlockReportParams,
    control::{BlockView, Created, Method, SpawnSpec},
    model::{BlockKind, StatusKind, StatusState},
};

use crate::agent::{AgentPane, AgentPaneEvent, AgentStatus};

/// The block's glyph (pane header, tab, New Tab row).
pub const ICON: &str = "settings-shapes";

/// `block.report` for a turn state.
#[must_use]
pub fn report(block_id: String, status: AgentStatus) -> Method {
    let (state, kind) = match status {
        AgentStatus::Idle => (StatusState::Idle, None),
        AgentStatus::Working => (StatusState::Working, None),
        AgentStatus::Blocked { question: false } => (StatusState::Blocked, Some(StatusKind::Permission)),
        AgentStatus::Blocked { question: true } => (StatusState::Blocked, Some(StatusKind::Question)),
        AgentStatus::Done => (StatusState::Done, None),
        AgentStatus::Error => (StatusState::Error, None),
    };
    Method::BlockReport(BlockReportParams {
        block_id,
        id: String::new(),
        state,
        kind,
        progress: None,
        app: Some("otto".into()),
        title: None,
        message: None,
    })
}

impl crate::workspace::Workspace {
    /// Agent blocks run on this Mac only: elsewhere they show as unsupported.
    pub(crate) fn agents_here(&self) -> bool {
        self.hosts.get(self.active_host).is_some_and(|h| h.info.local)
    }

    /// The agent pane of `block`, created on first sight.
    pub(crate) fn ensure_agent(&mut self, block: &str, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.agents.contains_key(block) {
            return;
        }
        let info = self.block_info(block);
        let (session, cwd) = (info.and_then(|i| i.agent.as_ref()).and_then(|a| a.session_id.clone()), info.and_then(|i| i.cwd.clone()));
        let (id, rt) = (block.to_owned(), self.rt.clone());
        let pane = cx.new(|cx| AgentPane::new(id, session, cwd, rt, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &AgentPaneEvent, window, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                AgentPaneEvent::Activated => this.pane_activated(block),
                AgentPaneEvent::Session { session_id, title } => {
                    this.call(Method::AgentSet { block_id: block, session_id: Some(session_id.clone()), title: Some(title.clone()) });
                }
                AgentPaneEvent::Status(status) => this.call(report(block, *status)),
                AgentPaneEvent::Reveal(target) => this.reveal_agent_target(target.clone(), window, cx),
            }
            cx.notify();
        });
        self.agents.insert(block.to_owned(), (pane, sub));
    }

    pub(crate) fn agent_focused(&self, window: &gpui::Window, cx: &gpui::App) -> Option<String> {
        self.agents.iter().find(|(_, (p, _))| p.read(cx).focus.contains_focused(window, cx)).map(|(id, _)| id.clone())
    }

    /// Jump to `block` named by a tool call, in whichever session holds it.
    fn reveal_agent_target(&mut self, block: String, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let host = self.active_host;
        if let Some(view) = self.view.as_ref().filter(|v| v.blocks.iter().any(|b| b.id == block)) {
            let session = view.session.id.clone();
            return self.reveal_block(host, Some(session), Some(block), window, cx);
        }
        let Some(control) = self.control() else { return };
        let id = block.clone();
        let task = self.rt.spawn(async move { control.call::<BlockView>(Method::BlockView { block_id: id }).await });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(view)) = task.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.active_host == host {
                    this.reveal_block(host, Some(view.block.session_id), Some(block), window, cx);
                }
            });
        })
        .detach();
    }

    /// New Agent: an agent block beside `block` in its cwd, composer focused.
    pub(crate) fn new_agent(&mut self, block: String, cx: &mut gpui::Context<Self>) {
        let spawn = SpawnSpec { cwd: self.cwd_of(&block, cx), command: Vec::new(), size: None };
        let axis = self.auto_axis(&block);
        self.create_agent(Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Agent, url: None }, cx);
    }

    /// New Agent Tab: a tab holding an agent block in the focused block's cwd.
    pub(crate) fn new_agent_tab(&mut self, window: &gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(view) = &self.view else { return };
        let session_id = view.session.id.clone();
        let cwd = self.focused_block(window, cx).and_then(|b| self.cwd_of(&b, cx));
        let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
        self.create_agent(Method::WindowCreate { session_id, spawn, kind: BlockKind::Agent, url: None }, cx);
    }

    fn create_agent(&mut self, method: Method, cx: &mut gpui::Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn(async move { control.call::<Created>(method).await });
        cx.spawn(async move |this, cx| {
            let created = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| match created {
                Ok(created) => this.composer_pending = Some(created.block_id),
                Err(error) => {
                    tracing::warn!("new agent: {error:#}");
                    this.flash(this.failed(&error, "Couldn't open an agent"), cx);
                }
            });
        })
        .detach();
    }

    /// Focus the pending new agent's composer once its pane exists.
    pub(crate) fn focus_new_agent(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(pane) = self.composer_pending.as_ref().and_then(|b| self.agents.get(b)).map(|(p, _)| p.clone()) else { return };
        self.composer_pending = None;
        pane.update(cx, |pane, cx| pane.focus_composer(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_reports() {
        let kinds = |status| match report("b".into(), status) {
            Method::BlockReport(p) => (p.state, p.kind),
            _ => unreachable!(),
        };
        assert_eq!(kinds(AgentStatus::Idle), (StatusState::Idle, None));
        assert_eq!(kinds(AgentStatus::Working), (StatusState::Working, None));
        assert_eq!(kinds(AgentStatus::Blocked { question: false }), (StatusState::Blocked, Some(StatusKind::Permission)));
        assert_eq!(kinds(AgentStatus::Blocked { question: true }), (StatusState::Blocked, Some(StatusKind::Question)));
        assert_eq!(kinds(AgentStatus::Done), (StatusState::Done, None));
        assert_eq!(kinds(AgentStatus::Error), (StatusState::Error, None));
    }
}
