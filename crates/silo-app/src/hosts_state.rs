//! Hosts shared by every window of the process: their connections,
//! client caches, last known trees and restored mirrors. Each connection
//! event is applied here once, then emitted to every window's
//! `Workspace`, which keeps a snapshot of `entries` (see `sync_hosts`).

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task};
use silo_client::{Control, transport::EndpointAddr};
use silo_proto::{
    control::{Event, Method},
    model::{BlockStatus, SessionSummary, SessionView},
};

use crate::{
    cache::{self, CacheWriter, CachedTree},
    conn::ConnEvent,
    hosts::{self, AppState, HostInfo, HostState, RemoteHost},
    notifications::{self, Seen},
};

/// What a window sees of a host.
#[derive(Clone)]
pub struct HostEntry {
    pub info: HostInfo,
    pub state: HostState,
    pub control: Option<Control>,
    /// Sessions, oldest first (the order of their ⌃⌘ shortcuts).
    pub sessions: Vec<SessionSummary>,
    /// Stop Server: the connection loop waits instead of respawning.
    pub paused: Arc<AtomicBool>,
    /// An incompatible local server holds this host.
    pub incompatible: Option<silo_client::IncompatibleServer>,
    /// Removed via Remove Host…; kept so host indices stay stable.
    pub removed: bool,
}

/// A host's process-wide resources.
struct Owned {
    conn: Option<Task<()>>,
    /// This host's client cache, its only writer (`None` if it could not
    /// be opened).
    cache: Option<CacheWriter>,
    /// Last known tree, shown while switching to it offline.
    tree: Option<CachedTree>,
    /// Mirrors waiting for their pane: from the cache, or kept in memory
    /// when a window switches away.
    restored: HashMap<String, silo_client::Mirror>,
}

/// A connection event of `host`, already applied to `HostsState`.
pub struct HostEvent {
    pub host: usize,
    pub event: ConnEvent,
}

/// How often hosts.json is checked for outside changes.
const HOSTS_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Changes that bring the remote hosts in line with hosts.json.
#[derive(Debug, Default, PartialEq, Eq)]
struct HostsPlan {
    add: Vec<RemoteHost>,
    forget: Vec<usize>,
    rename: Vec<(usize, String)>,
}

/// `current`: (index, EndpointId, name) of every live remote host.
fn reconcile(current: &[(usize, String, &str)], saved: Vec<RemoteHost>) -> HostsPlan {
    let mut plan = HostsPlan::default();
    for (index, id, name) in current {
        match saved.iter().find(|h| h.id == *id) {
            None => plan.forget.push(*index),
            Some(host) if host.name != *name => plan.rename.push((*index, host.name.clone())),
            Some(_) => {}
        }
    }
    plan.add = saved.into_iter().filter(|h| !current.iter().any(|(_, id, _)| *id == h.id)).collect();
    plan
}

pub struct HostsState {
    pub rt: tokio::runtime::Handle,
    pub home: PathBuf,
    entries: Vec<HostEntry>,
    owned: Vec<Owned>,
    /// App preferences (state.json); written only here.
    state: AppState,
    /// Machines offering to link (see `offers.rs`).
    pub(crate) offers: crate::offers::Offers,
    /// Owner actions waiting for the user (see `approvals.rs`).
    pub(crate) approvals: crate::approvals::Approvals,
    /// Notifications shown and not yet taken away, and when.
    delivered: HashMap<String, std::time::Instant>,
    /// Routes clicked notifications.
    _clicks: Option<Task<()>>,
    /// hosts.json's last seen (mtime, length); see `watch_hosts`.
    hosts_stamp: Option<(std::time::SystemTime, u64)>,
    _watch: Option<Task<()>>,
    _quit: Subscription,
}

impl EventEmitter<HostEvent> for HostsState {}
impl EventEmitter<crate::offers::OfferEvent> for HostsState {}

struct GlobalHosts(Entity<HostsState>);

impl Global for GlobalHosts {}

impl HostsState {
    /// Create the process's hosts (local, then saved remotes) and connect.
    pub fn install(rt: tokio::runtime::Handle, home: PathBuf, service: Result<silo_client::service::Service, String>, cx: &mut App) -> Entity<Self> {
        let hosts = cx.new(|cx| {
            let quit = cx.on_app_quit(|this: &mut Self, _| {
                let writers: Vec<CacheWriter> = this.owned.iter().filter_map(|h| h.cache.clone()).collect();
                let flush = move || writers.iter().for_each(|w| w.flush(std::time::Duration::from_secs(2)));
                // Deferred: windows queue their final checkpoints first.
                let rt = this.rt.clone();
                async move {
                    let _ = rt.spawn_blocking(flush).await;
                }
            });
            let state = hosts::load_state(&home);
            crate::theme::apply_ui_font(&state.ui_font_family, cx);
            let offers = crate::offers::Offers::default();
            let (clicks_tx, mut clicks) = tokio::sync::mpsc::unbounded_channel();
            #[cfg(debug_assertions)]
            test_clicks(&rt, clicks_tx.clone());
            notifications::install(clicks_tx);
            notifications::set_badge(0);
            let clicks = cx.spawn(async move |_, cx| {
                while let Some(target) = clicks.recv().await {
                    cx.update(|cx| reveal(&target, cx));
                }
            });
            let mut this = Self {
                rt,
                home: home.clone(),
                entries: Vec::new(),
                owned: Vec::new(),
                state,
                offers,
                approvals: crate::approvals::Approvals::default(),
                delivered: HashMap::new(),
                _clicks: Some(clicks),
                hosts_stamp: hosts::stamp(&home),
                _watch: None,
                _quit: quit,
            };
            this.add(HostInfo::local(home.clone(), service), cx);
            for remote in hosts::load(&home) {
                this.add(HostInfo::remote(home.clone(), &remote), cx);
            }
            for (name, ticket) in crate::share::load(&home) {
                this.add(HostInfo::shared(home.clone(), name, ticket), cx);
            }
            this._watch = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(HOSTS_POLL).await;
                    if this.update(cx, |this, cx| this.watch_hosts(cx)).is_err() {
                        return;
                    }
                }
            }));
            this
        });
        cx.set_global(GlobalHosts(hosts.clone()));
        hosts
    }

    /// hosts.json changed (e.g. `silo remote install`): connect and
    /// announce new hosts, drop removed ones, take new names.
    fn watch_hosts(&mut self, cx: &mut Context<Self>) {
        let stamp = hosts::stamp(&self.home);
        if stamp == self.hosts_stamp {
            return;
        }
        // Mid-write or invalid: keep the hosts, look again next time.
        let Some(saved) = hosts::try_load(&self.home) else { return };
        self.hosts_stamp = stamp;
        let current: Vec<(usize, String, &str)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, h)| !h.removed)
            .filter_map(|(i, h)| Some((i, h.info.transport.remote_id()?, h.info.name.as_str())))
            .collect();
        let plan = reconcile(&current, saved);
        for index in plan.forget {
            self.forget(index, cx);
        }
        for (index, name) in plan.rename {
            self.rename(index, name, cx);
        }
        for host in plan.add {
            let index = self.add(HostInfo::remote(self.home.clone(), &host), cx);
            cx.emit(crate::offers::OfferEvent::Added { host: index, name: host.name, invite: None });
        }
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalHosts>().0.clone()
    }

    pub fn app_state(&self) -> &AppState {
        &self.state
    }

    /// Change the app preferences; saved (and every window notified) only
    /// when they changed.
    pub fn update_state(&mut self, change: impl FnOnce(&mut AppState), cx: &mut Context<Self>) {
        let mut state = self.state.clone();
        change(&mut state);
        if state != self.state {
            hosts::save_state(&self.home, &state);
            self.state = state;
            cx.notify();
        }
    }

    pub fn entries(&self) -> &[HostEntry] {
        &self.entries
    }

    pub fn cache(&self, host: usize) -> Option<CacheWriter> {
        self.owned.get(host)?.cache.clone()
    }

    pub fn tree(&self, host: usize) -> Option<&CachedTree> {
        self.owned.get(host)?.tree.as_ref()
    }

    /// Remember `host`'s live tree (in memory and its cache).
    pub fn save_tree(&mut self, host: usize, tree: CachedTree) {
        let owned = &mut self.owned[host];
        if let Some(writer) = &owned.cache {
            writer.save_tree(tree.clone());
        }
        owned.tree = Some(tree);
    }

    /// `session` was deleted on `host`: never paint it again, nor list it
    /// offline.
    fn forget_session(&mut self, host: usize, session: &str) {
        let owned = &mut self.owned[host];
        let Some(tree) = owned.tree.as_mut() else { return };
        if tree.view.session.id == session {
            owned.tree = None;
            if let Some(writer) = &owned.cache {
                writer.clear_tree();
            }
        } else if tree.sessions.iter().any(|s| s.id == session) {
            tree.sessions.retain(|s| s.id != session);
            if let Some(writer) = &owned.cache {
                writer.save_tree(tree.clone());
            }
        }
    }

    pub fn take_restored(&mut self, host: usize, block: &str) -> Option<silo_client::Mirror> {
        self.owned[host].restored.remove(block)
    }

    pub fn put_restored(&mut self, host: usize, mirrors: impl IntoIterator<Item = (String, silo_client::Mirror)>) {
        self.owned[host].restored.extend(mirrors);
    }

    /// Closed blocks: drop their checkpoints and waiting mirrors.
    pub fn forget_blocks(&mut self, host: usize, closed: Vec<String>) {
        let owned = &mut self.owned[host];
        owned.restored.retain(|id, _| !closed.contains(id));
        if let Some(writer) = &owned.cache {
            writer.forget(closed);
        }
    }

    /// Add a host, open its cache and start its connection; its index.
    pub fn add(&mut self, info: HostInfo, cx: &mut Context<Self>) -> usize {
        let index = self.entries.len();
        let opened = cache::open_host(&cache::root(), &info.transport.cache_identity());
        let (cache, tree, restored) = opened.map_or((None, None, HashMap::new()), |c| (Some(c.writer), c.tree, c.restored));
        let paused = Arc::new(AtomicBool::new(false));
        let mut events = crate::conn::start(&self.rt, info.transport.clone(), paused.clone());
        let conn = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update(cx, |this, cx| this.conn_event(index, event, cx)).is_err() {
                    return;
                }
            }
        });
        let sessions = tree.as_ref().map(|t: &CachedTree| t.sessions.clone()).unwrap_or_default();
        let state = HostState::Connecting;
        self.entries.push(HostEntry { info, state, control: None, sessions, paused, incompatible: None, removed: false });
        self.owned.push(Owned { conn: Some(conn), cache, tree, restored });
        cx.notify();
        index
    }

    /// Disconnect a removed (or re-paired) host and close its tunnel.
    pub fn forget(&mut self, host: usize, cx: &mut Context<Self>) {
        let entry = &mut self.entries[host];
        if let Some(id) = entry.info.transport.remote_id() {
            self.rt.spawn(crate::web::close_tunnel(id));
        }
        entry.removed = true;
        entry.control = None;
        self.owned[host].conn = None;
        self.host_down(host);
        cx.notify();
    }

    /// Persist a paired host (see [`hosts::save`] for its name), replace
    /// the old connection `replace` and connect to it; its index and name.
    pub fn save_remote(
        &mut self,
        addr: EndpointAddr,
        chosen: Option<String>,
        suggested: &str,
        replace: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Result<(usize, String), String> {
        let host = hosts::update(&self.home, |saved| hosts::save(saved, addr, chosen, suggested)).map_err(|e| format!("Could not save: {e:#}"))?;
        if let Some(index) = replace.filter(|i| *i < self.entries.len()) {
            self.forget(index, cx);
        }
        Ok((self.add(HostInfo::remote(self.home.clone(), &host), cx), host.name))
    }

    /// Keep a shared link (`shares.json`) and connect to it; its index. A
    /// link already open is shown again, not added twice.
    pub fn save_share(&mut self, name: String, ticket: silo_client::transport::ShareTicket, cx: &mut Context<Self>) -> Result<usize, String> {
        let info = HostInfo::shared(self.home.clone(), name, ticket.clone());
        if let Some(index) = self.entries.iter().position(|h| !h.removed && h.info.key() == info.key()) {
            return Ok(index);
        }
        crate::share::save(&self.home, &info.name, &ticket).map_err(|e| format!("Could not save: {e:#}"))?;
        Ok(self.add(info, cx))
    }

    pub fn rename(&mut self, host: usize, name: String, cx: &mut Context<Self>) {
        self.entries[host].info.name = name;
        cx.notify();
    }

    fn conn_event(&mut self, host: usize, mut event: ConnEvent, cx: &mut Context<Self>) {
        // A shared link refused is revoked or expired, for good (the
        // connection loop has stopped).
        if matches!(event, ConnEvent::NotPaired) && self.entries[host].info.transport.is_shared() {
            event = ConnEvent::Disconnected(crate::share::ENDED.into());
        }
        let entry = &mut self.entries[host];
        match &mut event {
            ConnEvent::Connected(control) => {
                entry.state = HostState::Connected;
                entry.control = Some(control.clone());
                entry.incompatible = None;
                self.refresh_sessions(host, cx);
                self.gc_cache(host);
            }
            ConnEvent::Disconnected(reason) => {
                entry.control = None;
                entry.incompatible = None;
                entry.state = HostState::Unreachable(reason.clone());
                self.host_down(host);
            }
            ConnEvent::Quit => cx.quit(),
            ConnEvent::Resync => self.refresh_sessions(host, cx),
            ConnEvent::Addr(addr) => self.save_addr(host, addr),
            ConnEvent::NotPaired => {
                (entry.control, entry.state) = (None, HostState::NotPaired);
                self.host_down(host);
            }
            ConnEvent::Incompatible => {
                (entry.control, entry.state) = (None, HostState::Incompatible);
                self.host_down(host);
            }
            ConnEvent::IncompatibleServer(server) => {
                entry.control = None;
                entry.sessions.clear();
                entry.state = HostState::Unreachable(server.headline());
                entry.incompatible = Some(server.clone());
                self.host_down(host);
            }
            ConnEvent::Event(Event::SessionsChanged { sessions }) => {
                sessions.sort_by_key(|s| s.created_at);
                entry.sessions.clone_from(sessions);
            }
            ConnEvent::Event(Event::SessionClosed { session_id }) => {
                self.forget_session(host, session_id);
                self.gc_cache(host);
            }
            ConnEvent::Event(Event::HostOffered { offer }) => self.host_offered(host, offer, cx),
            ConnEvent::Event(Event::BlockStatus { status }) => self.block_status(host, status),
            ConnEvent::Event(Event::BlockAttention { block_id, session_id, session_name, block_title, reason, title, body, .. }) => {
                let info = &self.entries[host].info;
                let (key, name) = (info.key(), info.name.clone());
                let host_on = !self.state.quiet_hosts.contains(&key);
                let seen = seen(cx, host, session_id, block_id);
                let notify = notifications::wanted(&self.state.notifications, host_on, *reason, seen);
                tracing::debug!(host = name, block = block_id, ?reason, ?seen, host_on, notify, "attention");
                if notify {
                    let note = notifications::attention_note(&name, &key, session_id, session_name, block_id, block_title, *reason, title, body);
                    let now = std::time::Instant::now();
                    if notifications::repeat(&self.delivered, &note.id, now) {
                        tracing::debug!(id = note.id, "attention: its notification is still showing");
                    } else {
                        self.delivered.insert(note.id.clone(), now);
                        notifications::deliver(note);
                    }
                }
            }
            ConnEvent::Event(Event::ApprovalRequested { id, summary, caller, expires_in_ms, .. }) => {
                self.approvals.requested(host, id, summary, caller, *expires_in_ms, std::time::Instant::now());
                let info = &self.entries[host].info;
                let key = info.key();
                // The card is in the key window; away from Silo, say so.
                if cx.active_window().is_none() && self.state.notifications.on && !self.state.quiet_hosts.contains(&key) {
                    let card = self.approvals.cards.last().expect("just added");
                    let note = notifications::Note {
                        id: notifications::approval_identifier(&key, id),
                        title: format!("{} · approval", info.name),
                        subtitle: card.asks(),
                        body: card.summary.clone(),
                        target: notifications::Target { host: key, session_id: None, block_id: None },
                    };
                    self.delivered.insert(note.id.clone(), std::time::Instant::now());
                    notifications::deliver(note);
                }
            }
            ConnEvent::Event(Event::ApprovalResolved { id, .. }) => {
                self.approvals.resolved(host, id);
                let note = notifications::approval_identifier(&self.entries[host].info.key(), id);
                if self.delivered.remove(&note).is_some() {
                    notifications::remove(vec![note]);
                }
            }
            ConnEvent::Event(_) => {}
        }
        self.update_badge();
        cx.emit(HostEvent { host, event });
    }

    /// `host` is gone (disconnected, removed): its waiting approvals failed
    /// with it.
    fn host_down(&mut self, host: usize) {
        self.approvals.host_gone(host);
    }

    /// A block's new status: notifications of records gone from it (seen
    /// on any device) are taken away.
    fn block_status(&mut self, host: usize, status: &BlockStatus) {
        let key = self.entries[host].info.key();
        let stale = notifications::stale(&self.delivered, &key, status);
        for id in &stale {
            self.delivered.remove(id);
        }
        notifications::remove(stale);
    }

    /// Blocked + error records across every connected host, on the Dock.
    fn update_badge(&self) {
        let count: u32 = self
            .entries
            .iter()
            .filter(|h| !h.removed && h.control.is_some())
            .flat_map(|h| &h.sessions)
            .map(|s| s.attention.blocked + s.attention.error)
            .sum();
        notifications::set_badge(count);
    }

    /// Whether `host`'s notifications are on (Settings › Hosts).
    pub fn notifies(&self, host: usize) -> bool {
        self.entries.get(host).is_some_and(|h| !self.state.quiet_hosts.contains(&h.info.key()))
    }

    /// Turn `host`'s notifications on or off.
    pub fn set_notifies(&mut self, host: usize, on: bool, cx: &mut Context<Self>) {
        let Some(key) = self.entries.get(host).map(|h| h.info.key()) else { return };
        tracing::debug!(host, on, "host notifications");
        self.update_state(
            |s| {
                s.quiet_hosts.retain(|k| *k != key);
                if !on {
                    s.quiet_hosts.push(key);
                }
            },
            cx,
        );
    }

    /// A remote server's new route (already in hosts.json): used now.
    fn save_addr(&mut self, host: usize, addr: &silo_client::transport::EndpointAddr) {
        if let hosts::Transport::Iroh { addr: current, .. } = &mut self.entries[host].info.transport {
            current.clone_from(addr);
        }
    }

    fn refresh_sessions(&mut self, host: usize, cx: &mut Context<Self>) {
        let Some(control) = self.entries[host].control.clone() else {
            return;
        };
        let task = self.rt.spawn(async move { control.call::<Vec<SessionSummary>>(Method::SessionList {}).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(mut sessions)) = task.await {
                sessions.sort_by_key(|s| s.created_at);
                let _ = this.update(cx, |this, cx| {
                    this.entries[host].sessions = sessions;
                    this.update_badge();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Drop checkpoints of blocks `host`'s server no longer has, checked
    /// against every block of every session.
    fn gc_cache(&self, host: usize) {
        let (Some(writer), Some(control)) = (self.owned[host].cache.clone(), self.entries[host].control.clone()) else {
            return;
        };
        self.rt.spawn(async move {
            let all = async {
                let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
                let mut live = Vec::new();
                for s in sessions {
                    let view: SessionView = control.call(Method::SessionView { session_id: s.id }).await?;
                    live.extend(cache::live_blocks(&view));
                }
                anyhow::Ok(live)
            };
            match all.await {
                Ok(live) => writer.gc(live),
                Err(error) => tracing::debug!("cache gc skipped: {error:#}"),
            }
        });
    }
}

/// How much of `host`'s block the user sees across Silo's windows.
fn seen(cx: &App, host: usize, session: &str, block: &str) -> Seen {
    let key = cx.active_window();
    let mut seen = Seen::Hidden;
    for handle in cx.windows() {
        let Some(workspace) = handle.downcast::<crate::workspace::Workspace>() else { continue };
        let Ok(ws) = workspace.read(cx) else { continue };
        if ws.active_host != host || ws.view.as_ref().is_none_or(|v| v.session.id != session) {
            continue;
        }
        let Some(w) = ws.active_window() else { continue };
        let shown = if w.zoomed { w.focused == block } else { crate::floats::window_blocks(w).iter().any(|b| b == block) };
        if !shown || key.is_none() {
            continue;
        }
        if key == Some(handle) && w.focused == block {
            return Seen::Looking;
        }
        seen = Seen::Visible;
    }
    seen
}

/// Debug builds, for UI tests where macOS won't grant notifications to a
/// dev bundle unattended: `SILO_TEST_NOTIFICATION_CLICK=<file>` makes a
/// line `host session block` written there count as a click on such a
/// notification (the file is removed once read).
#[cfg(debug_assertions)]
fn test_clicks(rt: &tokio::runtime::Handle, clicks: tokio::sync::mpsc::UnboundedSender<notifications::Target>) {
    let Some(path) = std::env::var_os("SILO_TEST_NOTIFICATION_CLICK").map(PathBuf::from) else { return };
    rt.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let _ = std::fs::remove_file(&path);
            let mut words = text.split_whitespace().map(str::to_owned);
            let target = notifications::target(words.next(), words.next(), words.next());
            tracing::info!(?target, "notification clicked (test)");
            if let Some(target) = target
                && clicks.send(target).is_err()
            {
                return;
            }
        }
    });
}

/// A clicked notification: bring Silo forward and show its host, session
/// and block in the frontmost window.
fn reveal(target: &notifications::Target, cx: &mut App) {
    let shared = HostsState::global(cx);
    let Some(host) = shared.read(cx).entries.iter().position(|h| !h.removed && h.info.key() == target.host) else {
        tracing::info!(host = target.host, "notification for a host no longer here");
        return;
    };
    let frontmost = cx.window_stack().unwrap_or_else(|| cx.windows()).into_iter().find_map(|w| w.downcast::<crate::workspace::Workspace>());
    let Some(handle) = frontmost.or_else(|| crate::open_window(None, cx).ok()) else { return };
    let _ = handle.update(cx, |workspace, window, cx| {
        window.activate_window();
        workspace.reveal_block(host, target.session_id.clone(), target.block_id.clone(), window, cx);
    });
    cx.activate(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::tests::{addr, temp};

    #[test]
    fn hosts_file_changes_become_a_plan() {
        let dir = temp("reconcile");
        let (a, b, c) = (
            RemoteHost::new(addr(&dir.join("a")), "a".into()),
            RemoteHost::new(addr(&dir.join("b")), "b".into()),
            RemoteHost::new(addr(&dir.join("c")), "c".into()),
        );
        let current = vec![(1, a.id.clone(), "a"), (2, b.id.clone(), "b")];
        // The app's own writes match what it has: nothing to do.
        assert_eq!(reconcile(&current, vec![a.clone(), b.clone()]), HostsPlan::default());
        // The CLI added c, removed b and renamed a.
        let renamed = RemoteHost { name: "mini".into(), ..a.clone() };
        let plan = reconcile(&current, vec![renamed, c.clone()]);
        assert_eq!(plan, HostsPlan { add: vec![c], forget: vec![2], rename: vec![(1, "mini".into())] });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_hosts_file_is_not_an_empty_one() {
        let dir = temp("try-load");
        assert_eq!(hosts::try_load(&dir), Some(Vec::new()), "missing: no hosts");
        assert_eq!(hosts::stamp(&dir), None);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hosts.json"), b"{\"hosts\": [").unwrap();
        assert_eq!(hosts::try_load(&dir), None, "half written");
        assert!(hosts::stamp(&dir).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
