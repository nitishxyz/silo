//! Workspace wiring of agent blocks: an [`AgentPane`] per agent block of a
//! host with otto, its events sent to the server (`docs/AGENT_BLOCK.md`).

use gpui::AppContext as _;
use silo_proto::{
    agent::BlockReportParams,
    compat::feature,
    control::{BlockView, Created, Method, Reveal, SpawnSpec},
    model::{AgentState, BlockInfo, BlockKind, StatusKind, StatusState},
};

use crate::{
    agent::{AgentPane, AgentPaneEvent, AgentStatus, client::Otto},
    hosts::Transport,
    settings_agents::Place,
};

/// The block's glyph (pane header, tab, New Tab row).
pub const ICON: &str = "agent";

/// How a host is reached, as far as agents care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Local,
    Iroh,
    Shared,
}

impl Reach {
    pub(crate) fn of(transport: &Transport) -> Self {
        match transport {
            Transport::Local { .. } => Self::Local,
            Transport::Iroh { .. } => Self::Iroh,
            Transport::Shared { .. } => Self::Shared,
        }
    }
}

/// Whether a host runs agent blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agents {
    Run,
    /// A paired remote not connected yet: its blocks wait for it, nothing new is offered.
    Unknown,
    No,
}

/// Agents on a host reached by `reach` whose server advertised `features`
/// (`None`: not connected): this Mac always, a paired remote once its
/// server says it tunnels to otto, a shared link never.
pub(crate) fn agents_supported(reach: Reach, features: Option<&[String]>) -> Agents {
    match (reach, features) {
        (Reach::Local, _) => Agents::Run,
        (Reach::Iroh, None) => Agents::Unknown,
        (Reach::Iroh, Some(f)) if f.iter().any(|f| f == feature::OTTO) => Agents::Run,
        (Reach::Iroh | Reach::Shared, _) => Agents::No,
    }
}

/// Settings › Agents for a host named `host` whose daemon is `otto`.
pub(crate) fn settings_place(agents: Agents, otto: Option<Otto>, host: &str) -> Place {
    match (agents, otto) {
        (Agents::Run, Some(Otto::Local)) => Place::Local,
        (Agents::Run, Some(Otto::Remote { id })) => Place::Remote { id, host: host.to_owned() },
        (Agents::Unknown, Some(_)) => Place::Offline { host: host.to_owned() },
        _ => Place::Unsupported { host: host.to_owned() },
    }
}

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
    /// [`agents_supported`] for `host`.
    fn agents(&self, host: usize) -> Agents {
        self.hosts
            .get(host)
            .map_or(Agents::No, |h| agents_supported(Reach::of(&h.info.transport), h.control.as_ref().map(|c| c.hello().features.as_slice())))
    }

    /// Whether `host`'s agent blocks show as agents (an unconnected remote's
    /// wait for it); elsewhere they show as unsupported.
    pub(crate) fn agents_on(&self, host: usize) -> bool {
        self.agents(host) != Agents::No
    }

    /// Whether New Agent is offered on `host`: it runs agents, as far as is known now.
    pub(crate) fn agents_offered(&self, host: usize) -> bool {
        self.agents(host) == Agents::Run
    }

    /// Why New Agent isn't offered on the active host, if it isn't.
    pub(crate) fn agents_not_offered(&self) -> Option<String> {
        match self.agents(self.active_host) {
            Agents::Run => None,
            Agents::Unknown => Some(format!("{} is not connected", self.hosts[self.active_host].info.name)),
            Agents::No => Some(self.not_supported()),
        }
    }

    /// The otto daemon of `host`'s agent blocks, if it runs them.
    pub(crate) fn agent_target(&self, host: usize) -> Option<Otto> {
        self.agents_on(host).then(|| Otto::of(&self.hosts[host].info.transport)).flatten()
    }

    /// Settings › Agents for the active host.
    pub(crate) fn agents_place(&self) -> Place {
        let host = &self.hosts[self.active_host].info;
        settings_place(self.agents(self.active_host), Otto::of(&host.transport), &host.name)
    }

    /// `host`'s connection came or went: its agent panes show or drop the
    /// reconnect strip, and reconnect at once when it is back.
    pub(crate) fn agents_host_up(&mut self, host: usize, up: bool, cx: &mut gpui::Context<Self>) {
        if host != self.active_host {
            return;
        }
        for (pane, _) in self.agents.values() {
            pane.update(cx, |pane, cx| pane.set_host_up(up, cx));
        }
    }

    /// The agent pane of `block`, created on first sight.
    pub(crate) fn ensure_agent(&mut self, block: &str, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.agents.contains_key(block) {
            return;
        }
        let Some(target) = self.agent_target(self.active_host) else { return };
        let host = Some(&self.hosts[self.active_host].info).filter(|h| !h.local).map(|h| h.name.clone());
        let up = self.control().is_some();
        let info = self.block_info(block);
        let (session, cwd) = (info.and_then(|i| i.agent.as_ref()).and_then(|a| a.session_id.clone()), info.and_then(|i| i.cwd.clone()));
        let (id, rt) = (block.to_owned(), self.rt.clone());
        let pane = cx.new(|cx| AgentPane::new(id, session, cwd, rt, target, host, up, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &AgentPaneEvent, window, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                AgentPaneEvent::Activated => this.pane_activated(block),
                AgentPaneEvent::Session { session_id, title } => {
                    this.call(Method::AgentSet { block_id: block, session_id: Some(session_id.clone()), title: Some(title.clone()), cwd: None });
                }
                AgentPaneEvent::Project { cwd } => {
                    this.call(Method::AgentSet { block_id: block, session_id: None, title: None, cwd: Some(cwd.clone()) });
                }
                AgentPaneEvent::Status(status) => this.call(report(block, *status)),
                AgentPaneEvent::Reveal(target) => this.reveal_agent_target(target.clone(), window, cx),
                AgentPaneEvent::Stop(target) => this.call(Method::BlockClose { block_id: target.clone(), force: true }),
                AgentPaneEvent::OpenSession { session_id, title } => {
                    this.open_agent_session(block, session_id.clone(), title.clone(), window, cx);
                }
                AgentPaneEvent::Header => {}
            }
            cx.notify();
        });
        self.agents.insert(block.to_owned(), (pane, sub));
    }

    pub(crate) fn agent_focused(&self, window: &gpui::Window, cx: &gpui::App) -> Option<String> {
        self.agents.iter().find(|(_, (p, _))| p.read(cx).focus.contains_focused(window, cx)).map(|(id, _)| id.clone())
    }

    /// Jump to `block` named by a tool call, in whichever session holds it;
    /// a parked block (a command, a hidden terminal) floats in first.
    fn reveal_agent_target(&mut self, block: String, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let host = self.active_host;
        let parked = self.view.as_ref().is_some_and(|v| v.session.parked.iter().any(|p| p.id == block));
        if !parked && let Some(view) = self.view.as_ref().filter(|v| v.blocks.iter().any(|b| b.id == block)) {
            let session = view.session.id.clone();
            return self.reveal_block(host, Some(session), Some(block), window, cx);
        }
        let Some(control) = self.control() else { return };
        let id = block.clone();
        let task = self.rt.spawn(async move {
            // Not parked (or no longer): `bad_request`, and it is shown where it is.
            let _ = control.call_value(Method::BlockReveal { block_id: id.clone(), reveal: Reveal::Float }).await;
            control.call::<BlockView>(Method::BlockView { block_id: id }).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(view)) = task.await else {
                let _ = this.update(cx, |this, cx| this.flash(crate::status::Status::warning("That block has closed"), cx));
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.active_host == host {
                    this.reveal_block(host, Some(view.block.session_id), Some(block), window, cx);
                }
            });
        })
        .detach();
    }

    /// Show otto session `session` (a sub-agent, or its parent): focus the
    /// block already showing it, else split a new agent block beside `block`
    /// in its folder, showing it.
    fn open_agent_session(&mut self, block: String, session: String, title: String, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let host = self.active_host;
        if let Some(view) = &self.view
            && let Some(shown) = showing(&view.blocks, &session)
        {
            let silo_session = view.session.id.clone();
            return self.reveal_block(host, Some(silo_session), Some(shown), window, cx);
        }
        let spawn = SpawnSpec { cwd: self.cwd_of(&block, cx), command: Vec::new(), size: None };
        let axis = self.auto_axis(&block);
        // The block shows the session from its first frame.
        let agent = Some(AgentState { session_id: Some(session), title });
        self.create_agent(Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Agent, url: None, agent }, cx);
    }

    /// New Agent: an agent block beside `block` in its cwd, composer focused.
    pub(crate) fn new_agent(&mut self, block: String, cx: &mut gpui::Context<Self>) {
        let spawn = SpawnSpec { cwd: self.cwd_of(&block, cx), command: Vec::new(), size: None };
        let axis = self.auto_axis(&block);
        self.create_agent(Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Agent, url: None, agent: None }, cx);
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

/// The agent block showing otto session `session`, if any.
fn showing(blocks: &[BlockInfo], session: &str) -> Option<String> {
    blocks
        .iter()
        .find(|b| b.kind == BlockKind::Agent && b.agent.as_ref().and_then(|a| a.session_id.as_deref()) == Some(session))
        .map(|b| b.id.clone())
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

    #[test]
    fn new_agent_is_offered_on_this_mac_and_connected_remotes_with_otto() {
        let otto = vec![feature::STATUS.to_owned(), feature::OTTO.to_owned()];
        let older = vec![feature::STATUS.to_owned()];
        for (reach, features, agents) in [
            (Reach::Local, None, Agents::Run),
            (Reach::Local, Some(&older), Agents::Run),
            (Reach::Iroh, Some(&otto), Agents::Run),
            (Reach::Iroh, Some(&older), Agents::No),
            (Reach::Iroh, None, Agents::Unknown),
            (Reach::Shared, Some(&otto), Agents::No),
            (Reach::Shared, None, Agents::No),
        ] {
            assert_eq!(agents_supported(reach, features.map(Vec::as_slice)), agents, "{reach:?} {features:?}");
        }
        // Offered only where they surely run; an unconnected remote's blocks still wait for it.
        let offered = |reach, features: Option<&Vec<String>>| agents_supported(reach, features.map(Vec::as_slice)) == Agents::Run;
        assert!(offered(Reach::Local, None) && offered(Reach::Iroh, Some(&otto)));
        assert!(!offered(Reach::Iroh, None) && !offered(Reach::Iroh, Some(&older)) && !offered(Reach::Shared, Some(&otto)));
    }

    #[test]
    fn settings_agents_go_to_the_hosts_otto_or_say_why_not() {
        let remote = || Some(Otto::Remote { id: "abc".into() });
        assert_eq!(settings_place(Agents::Run, Some(Otto::Local), "This Mac"), Place::Local);
        let mini = settings_place(Agents::Run, remote(), "mini");
        assert_eq!(mini, Place::Remote { id: "abc".into(), host: "mini".into() });
        assert_eq!(mini.title(), "Agents on mini");
        assert_eq!(Place::Local.title(), "Agents");
        // Never this Mac's otto in their place.
        let old = settings_place(Agents::No, remote(), "mini");
        assert_eq!((old.target(), old.blocked().as_deref()), (None, Some("Not supported by mini")));
        let shared = settings_place(Agents::No, None, "Ana's link");
        assert_eq!((shared.target(), shared.blocked().as_deref()), (None, Some("Not supported by Ana's link")));
        let offline = settings_place(Agents::Unknown, remote(), "mini");
        assert_eq!((offline.target(), offline.blocked().as_deref()), (None, Some("mini is not connected")));
        assert_eq!(mini.target(), remote());
        assert_eq!(Place::Local.target(), Some(Otto::Local));
    }

    #[test]
    fn an_open_session_is_found_by_its_agent_block() {
        let block = |id: &str, kind: &str, session: Option<&str>| -> BlockInfo {
            serde_json::from_value(serde_json::json!({
                "id": id, "session_id": "s", "kind": kind, "title": "", "size": {"columns": 80, "rows": 24},
                "agent": (kind == "agent").then(|| serde_json::json!({"session_id": session, "title": ""})),
            }))
            .unwrap()
        };
        let blocks =
            [block("b1", "terminal", None), block("b2", "agent", None), block("b3", "agent", Some("child")), block("b4", "agent", Some("main"))];
        assert_eq!(showing(&blocks, "child").as_deref(), Some("b3"));
        assert_eq!(showing(&blocks, "main").as_deref(), Some("b4"));
        assert_eq!(showing(&blocks, "other"), None);
    }
}
