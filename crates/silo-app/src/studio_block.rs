//! Studio blocks in the workspace (docs/STUDIO.md §4): one [`StudioPane`] per Studio block shown,
//! `studio.changed` routed to its store, creating new ones (New Studio Tab / Pane, ⌘P Open Studio), files
//! opened in a Files block beside it, and its linked agent: found (or split off it with the `studio` agent,
//! linked once it has a session), given mentions in its composer or a message.

use std::collections::{HashMap, HashSet};

use gpui::{AppContext as _, Context, Window};
use otto_proto::{Delivery, InputPart, MentionKind};
use silo_proto::{
    control::{Created, FilesSetParams, Method, SpawnSpec},
    model::{AgentLink, AgentState, Axis, BlockKind},
    studio::{StudioChange, StudioSetParams},
};

use crate::{
    studio::{StudioPane, StudioPaneEvent},
    workspace::Workspace,
};

pub const ICON: &str = "clapperboard";

/// The agent an agent block split off a Studio block starts with.
pub const AGENT: &str = "studio";

/// How long a split agent's pane gets to appear.
const PANE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const PANE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Agent blocks of Studio blocks: those whose pane takes the `studio` agent when it appears, and the
/// studio each one links to once it has a session.
#[derive(Default)]
pub(crate) struct StudioAgents {
    pending: HashSet<String>,
    links: HashMap<String, String>,
    /// Studio blocks made for a placed result's link, not shown yet: the canvas and node to reveal.
    reveals: HashMap<String, (String, String)>,
}

impl gpui::Global for StudioAgents {}

/// What the linked agent gets.
#[derive(Clone, Debug)]
pub(crate) enum Then {
    /// Chips in its composer, the caret after them.
    Mentions(Vec<(MentionKind, String)>),
    /// A message sent (queued when it works).
    Send(String),
}

/// `block.split` for a Studio block beside `block` at `cwd`.
pub fn studio_split(block: String, axis: Axis, cwd: Option<String>) -> Method {
    let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Studio, url: None, agent: None, ids: None }
}

impl Workspace {
    pub(crate) fn ensure_studio(&mut self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocks.studio(block).is_some() {
            return;
        }
        let info = self.block_info(block).cloned();
        let otto = self.agent_target(self.active_host);
        let (id, rt, control) = (block.to_owned(), self.rt.clone(), self.control());
        let local = matches!(self.hosts[self.active_host].info.transport, crate::hosts::Transport::Local { .. });
        let pane = cx.new(|cx| {
            let mut pane = StudioPane::new(id, info, rt, control, otto, window, cx);
            pane.local = local;
            pane
        });
        if let Some((canvas, node)) = cx.try_global::<StudioAgents>().and_then(|a| a.reveals.get(block)).cloned() {
            cx.global_mut::<StudioAgents>().reveals.remove(block);
            pane.update(cx, |p, _| p.reveal_later(canvas, node));
        }
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &StudioPaneEvent, window, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                StudioPaneEvent::Activated => this.pane_activated(block),
                StudioPaneEvent::Header => {}
                StudioPaneEvent::Agent(then) => this.studio_agent(block, then.clone(), window, cx),
                StudioPaneEvent::OpenFile(path) => this.open_studio_file(block, path.clone(), cx),
            }
            cx.notify();
        });
        self.blocks.insert(block.to_owned(), crate::blocks::BlockPane::Studio(pane), sub);
    }

    /// `studio.changed` for one of this window's Studio blocks.
    pub(crate) fn studio_changed(&mut self, block: &str, what: StudioChange, name: Option<String>, rev: Option<u64>, cx: &mut Context<Self>) {
        if let Some(pane) = self.blocks.studio(block) {
            let store = pane.read(cx).store.clone();
            store.update(cx, |s, cx| s.changed(what, name, rev, cx));
        }
    }

    /// New Studio Tab: a tab with a studio in the focused block's folder.
    pub(crate) fn new_studio_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let session_id = view.session.id.clone();
        let cwd = self.new_block_folder(window, cx);
        let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
        self.call(Method::WindowCreate { session_id, spawn, kind: BlockKind::Studio, url: None, background: false, ids: None });
    }

    /// New Studio Pane: a studio in `block`'s folder, beside it.
    pub(crate) fn new_studio(&mut self, block: String, cx: &mut Context<Self>) {
        let axis = self.auto_axis(&block);
        let cwd = self.cwd_of(&block, cx);
        self.call(studio_split(block, axis, cwd));
    }

    /// ⌘P Open Studio: this session's Studio block, else a new one beside `block` (a tab with none focused).
    pub(crate) fn open_studio_command(&mut self, block: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.kind == BlockKind::Studio).map(|b| b.id.clone()));
        match (existing, block) {
            (Some(studio), _) => self.reveal_agent_target(studio, window, cx),
            (None, Some(block)) => self.new_studio(block, cx),
            (None, None) => self.new_studio_tab(window, cx),
        }
    }

    /// The Studio pane `block` is, else this session's first one with a project.
    pub(crate) fn studio_for(&self, block: Option<&str>, cx: &gpui::App) -> Option<gpui::Entity<StudioPane>> {
        let ready = |p: &gpui::Entity<StudioPane>| p.read(cx).store.read(cx).project().is_some();
        block.and_then(|b| self.blocks.studio(b)).filter(|p| ready(p)).cloned().or_else(|| {
            let ids: Vec<String> = self.view.as_ref()?.blocks.iter().filter(|b| b.kind == BlockKind::Studio).map(|b| b.id.clone()).collect();
            ids.iter().filter_map(|id| self.blocks.studio(id)).find(|p| ready(p)).cloned()
        })
    }

    /// Toggle World: the focused Studio block's World view (else this session's studio's), or back to its canvas.
    pub(crate) fn toggle_world_command(&mut self, block: Option<String>, cx: &mut Context<Self>) {
        if let Some(pane) = self.studio_for(block.as_deref(), cx) {
            pane.update(cx, |p, cx| p.toggle_world(cx));
        }
    }

    /// `path` (root-relative) of studio `block` in a Files block beside it (Edit formats…, Reveal in Files).
    fn open_studio_file(&mut self, block: String, path: String, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let root = self.blocks.studio(&block).map(|p| p.read(cx).state.root.clone()).filter(|r| !r.is_empty());
        let split = crate::files_block::files_split(block.clone(), self.auto_axis(&block), root.or_else(|| self.cwd_of(&block, cx)));
        let task = self.rt.spawn(async move {
            let created: Created = control.call(split).await?;
            let set = FilesSetParams { block_id: created.block_id, active: Some(path), ..FilesSetParams::default() };
            control.call_value(Method::FilesSet(set)).await?;
            anyhow::Ok(())
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) | Err(error) = task.await.map_err(anyhow::Error::from) {
                let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't open the file"), cx));
            }
        })
        .detach();
    }

    /// Agent block `agent`'s placed-result link (STUDIO.md §6.5): `canvas` shown in the Studio block linked to it,
    /// else one whose root holds `cwd` (focused), else a new one split off `agent` at `cwd`; `node` (empty: none) is
    /// revealed and selected on it, once the canvas is read.
    pub(crate) fn open_studio_canvas(&mut self, agent: String, cwd: &str, canvas: String, node: &str, window: &mut Window, cx: &mut Context<Self>) {
        let holds = |root: &str| {
            let root = root.trim_end_matches('/');
            !root.is_empty() && (cwd == root || cwd.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')))
        };
        let linked = |p: &StudioPane| p.state.agent.as_ref().is_some_and(|l| l.block_id.as_deref() == Some(agent.as_str()));
        let panes: Vec<(String, gpui::Entity<StudioPane>)> = self.blocks.studio_panes().map(|(id, p)| (id.clone(), p.clone())).collect();
        let found = panes.iter().find(|(_, p)| linked(p.read(cx))).or_else(|| panes.iter().find(|(_, p)| holds(&p.read(cx).state.root)));
        if let Some((block, pane)) = found.cloned() {
            let node = node.to_owned();
            pane.update(cx, |p, cx| p.reveal_node(canvas, node, window, cx));
            return self.reveal_agent_target(block, window, cx);
        }
        let Some(control) = self.control() else { return };
        let split = studio_split(agent.clone(), self.auto_axis(&agent), Some(cwd.to_owned()));
        let reveal = (canvas.clone(), node.to_owned());
        let task = self.rt.spawn(async move {
            let created: Created = control.call(split).await?;
            let block = created.block_id.clone();
            let view = Some(silo_proto::studio::StudioView::Canvas);
            let set = StudioSetParams { block_id: created.block_id, view, active: Some(Some(canvas)), ..StudioSetParams::default() };
            control.call_value(Method::StudioSet(set)).await?;
            anyhow::Ok(block)
        });
        cx.spawn(async move |this, cx| match task.await.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok(block) => {
                let _ = this.update(cx, |this, cx| {
                    // The pane may not exist yet: it takes the node when it is made (`ensure_studio`).
                    match this.blocks.studio(&block).cloned() {
                        Some(pane) => pane.update(cx, |p, cx| {
                            p.reveal_later(reveal.0, reveal.1);
                            cx.notify();
                        }),
                        None => {
                            cx.default_global::<StudioAgents>().reveals.insert(block, reveal);
                        }
                    }
                });
            }
            Err(error) => {
                let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't open the canvas"), cx));
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ the linked agent

    /// Studio `block`'s agent gets `then`: its linked agent block (focused), else a `studio` agent split off it.
    fn studio_agent(&mut self, block: String, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.blocks.studio(&block).cloned() else { return };
        let (link, root) = {
            let p = pane.read(cx);
            (p.state.agent.clone(), p.state.root.clone())
        };
        let linked = link.as_ref().and_then(|l| l.block_id.clone()).filter(|b| self.blocks.agent(b).is_some());
        if let Some(agent) = linked {
            self.reveal_agent_target(agent.clone(), window, cx);
            let session = link.map(|l| l.otto_session).filter(|s| !s.is_empty());
            return self.agent_then(&block, &agent, session, then, window, cx);
        }
        if self.agent_target(self.active_host).is_none() {
            let note = self.agents_not_offered().unwrap_or_else(|| "Agents aren't available here".into());
            return self.flash(crate::status::Status::warning(note), cx);
        }
        let spawn = SpawnSpec { cwd: Some(root).filter(|r| !r.is_empty()).or_else(|| self.cwd_of(&block, cx)), command: Vec::new(), size: None };
        let axis = self.auto_axis(&block);
        let method = Method::BlockSplit {
            block_id: block.clone(),
            axis,
            spawn,
            kind: BlockKind::Agent,
            url: None,
            agent: Some(AgentState { session_id: None, title: "Studio".into() }),
            ids: None,
        };
        cx.default_global::<StudioAgents>();
        match self.instant_block(method, window, cx) {
            Ok(agent) => self.studio_agent_made(block, agent, then, window, cx),
            Err(method) => {
                let Some(control) = self.control() else { return };
                let task = self.rt.spawn(async move { control.call::<Created>(method).await });
                cx.spawn_in(window, async move |this, cx| {
                    let created = task.await.map_err(anyhow::Error::from).and_then(|r| r);
                    let _ = this.update_in(cx, |this, window, cx| match created {
                        Ok(created) => this.studio_agent_made(block, created.block_id, then, window, cx),
                        Err(error) => this.flash(this.failed(&error, "Couldn't open an agent"), cx),
                    });
                })
                .detach();
            }
        }
    }

    /// A `studio` agent block `agent` was split off studio `block`: once its pane is here, `then`.
    fn studio_agent_made(&mut self, block: String, agent: String, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        {
            let agents = cx.default_global::<StudioAgents>();
            agents.links.insert(agent.clone(), block.clone());
        }
        self.studio_agent_created(&agent, cx);
        cx.spawn_in(window, async move |this, cx| {
            let start = std::time::Instant::now();
            while start.elapsed() < PANE_WAIT {
                let done = this
                    .update_in(cx, |this, window, cx| {
                        if this.blocks.agent(&agent).is_none() {
                            return false;
                        }
                        this.agent_then(&block, &agent, None, then.clone(), window, cx);
                        true
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
                cx.background_executor().timer(PANE_POLL).await;
            }
        })
        .detach();
    }

    /// Agent block `agent` (shown here; on `session` when it has one) of studio `block` gets `then`.
    fn agent_then(&mut self, block: &str, agent: &str, session: Option<String>, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.blocks.agent(agent).cloned() else { return };
        match then {
            Then::Mentions(mentions) => pane.update(cx, |p, cx| p.insert_mentions(mentions, window, cx)),
            // A session that works: queued on it, the composer's draft left alone.
            Then::Send(text) if session.is_some() => {
                let (Some(target), Some(session)) = (self.agent_target(self.active_host), session) else { return };
                let task = self.rt.spawn(async move {
                    let c = crate::agent::client::connection(&target).await.map_err(|e| crate::agent::describe(&e))?;
                    c.prompt(&session, vec![InputPart::Text { text }], Delivery::Queue).await.map_err(|e| crate::agent::describe(&e))?;
                    Ok::<_, String>(())
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Err(e)) = task.await {
                        let _ = this.update(cx, |this, cx| this.flash(crate::status::Status::warning(e), cx));
                    }
                })
                .detach();
            }
            Then::Send(text) => {
                let task = pane.update(cx, |p, cx| p.send_when_ready(text, window, cx));
                let (block, agent) = (block.to_owned(), agent.to_owned());
                cx.spawn(async move |this, cx| {
                    if let Some(session) = task.await {
                        let _ = this.update(cx, |this, _| this.link_studio(&block, &agent, session));
                    }
                })
                .detach();
            }
        }
    }

    /// Studio `block` talks to agent block `agent` on otto `session` from now on.
    fn link_studio(&mut self, block: &str, agent: &str, session: String) {
        let link = AgentLink { otto_session: session, block_id: Some(agent.to_owned()) };
        self.call_for(
            block,
            Method::StudioSet(StudioSetParams { block_id: block.to_owned(), agent: Some(Some(link)), ..StudioSetParams::default() }),
        );
    }

    /// An agent block was split off a Studio block: its pane takes the `studio` agent (now, or when it appears).
    pub(crate) fn studio_agent_created(&mut self, agent: &str, cx: &mut Context<Self>) {
        match self.blocks.agent(agent).cloned() {
            Some(pane) => pane.update(cx, |p, cx| p.prefer_agent(AGENT, cx)),
            None => _ = cx.default_global::<StudioAgents>().pending.insert(agent.to_owned()),
        }
    }

    /// Agent block `agent`'s pane appeared: the `studio` agent if it was split off a studio.
    pub(crate) fn studio_agent_pane(&mut self, agent: &str, cx: &mut Context<Self>) {
        let pending = cx.try_global::<StudioAgents>().is_some_and(|a| a.pending.contains(agent));
        if pending && let Some(pane) = self.blocks.agent(agent).cloned() {
            cx.global_mut::<StudioAgents>().pending.remove(agent);
            pane.update(cx, |p, cx| p.prefer_agent(AGENT, cx));
        }
    }

    /// Agent block `agent` started session `session`: the studio it was split for links to it.
    pub(crate) fn studio_agent_session(&mut self, agent: &str, session: &str, cx: &mut Context<Self>) {
        // Split for a studio here, else a studio linked to this block on another session (it moved on).
        let split = cx.try_global::<StudioAgents>().and_then(|a| a.links.get(agent).cloned());
        let moved = || {
            self.blocks
                .studio_panes()
                .find(|(_, p)| p.read(cx).state.agent.as_ref().is_some_and(|l| l.block_id.as_deref() == Some(agent)))
                .map(|(id, _)| id.clone())
        };
        let Some(block) = split.or_else(moved) else { return };
        let linked = self.blocks.studio(&block).is_some_and(|p| p.read(cx).state.agent.as_ref().is_some_and(|l| l.otto_session == session));
        if !linked {
            self.link_studio(&block, agent, session.to_owned());
        }
    }

    /// Whether `method` splits an agent block off a Studio block (it then takes the `studio` agent).
    pub(crate) fn splits_off_studio(&self, method: &Method) -> bool {
        splits_off_studio(method, |block| self.block_kind(block))
    }
}

/// Whether `method` splits an agent block off a block whose kind (`kind_of`) is Studio.
fn splits_off_studio(method: &Method, kind_of: impl Fn(&str) -> crate::blocks::Kind) -> bool {
    matches!(method, Method::BlockSplit { block_id, kind: BlockKind::Agent, .. } if kind_of(block_id) == crate::blocks::Kind::Studio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Kind;

    fn split(from: &str, kind: BlockKind) -> Method {
        let spawn = SpawnSpec { cwd: None, command: Vec::new(), size: None };
        Method::BlockSplit { block_id: from.into(), axis: Axis::Horizontal, spawn, kind, url: None, agent: None, ids: None }
    }

    #[test]
    fn an_agent_split_off_a_studio_takes_the_studio_agent() {
        let kind_of = |b: &str| match b {
            "s" => Kind::Studio,
            "a" => Kind::Agent,
            _ => Kind::Terminal,
        };
        assert!(splits_off_studio(&split("s", BlockKind::Agent), kind_of));
        // Off another kind, or not an agent, or not a split: no.
        assert!(!splits_off_studio(&split("t", BlockKind::Agent), kind_of));
        assert!(!splits_off_studio(&split("a", BlockKind::Agent), kind_of));
        assert!(!splits_off_studio(&split("s", BlockKind::Terminal), kind_of));
        assert!(!splits_off_studio(&split("s", BlockKind::Studio), kind_of));
        assert!(!splits_off_studio(&Method::BlockClose { block_id: "s".into(), force: false }, kind_of));
        assert_eq!(AGENT, "studio");
    }

    #[test]
    fn a_studio_split_starts_in_the_folder_given() {
        let Method::BlockSplit { block_id, axis, spawn, kind, url, agent, ids } = studio_split("b".into(), Axis::Vertical, Some("/w".into())) else {
            panic!()
        };
        assert_eq!((block_id.as_str(), axis, kind, spawn.cwd.as_deref()), ("b", Axis::Vertical, BlockKind::Studio, Some("/w")));
        assert!(spawn.command.is_empty() && url.is_none() && agent.is_none() && ids.is_none());
    }
}
