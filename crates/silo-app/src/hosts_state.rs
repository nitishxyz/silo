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
    model::{SessionSummary, SessionView},
};

use crate::{
    cache::{self, CacheWriter, CachedTree},
    conn::ConnEvent,
    hosts::{self, AppState, HostInfo, HostState, RemoteHost},
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
            let mut this = Self {
                rt,
                home: home.clone(),
                entries: Vec::new(),
                owned: Vec::new(),
                state,
                offers,
                hosts_stamp: hosts::stamp(&home),
                _watch: None,
                _quit: quit,
            };
            this.add(HostInfo::local(home.clone(), service), cx);
            for remote in hosts::load(&home) {
                this.add(HostInfo::remote(home.clone(), &remote), cx);
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

    pub fn rename(&mut self, host: usize, name: String, cx: &mut Context<Self>) {
        self.entries[host].info.name = name;
        cx.notify();
    }

    fn conn_event(&mut self, host: usize, mut event: ConnEvent, cx: &mut Context<Self>) {
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
            }
            ConnEvent::Quit => cx.quit(),
            ConnEvent::Resync => self.refresh_sessions(host, cx),
            ConnEvent::Addr(addr) => self.save_addr(host, addr),
            ConnEvent::NotPaired => (entry.control, entry.state) = (None, HostState::NotPaired),
            ConnEvent::Incompatible => (entry.control, entry.state) = (None, HostState::Incompatible),
            ConnEvent::IncompatibleServer(server) => {
                entry.control = None;
                entry.sessions.clear();
                entry.state = HostState::Unreachable(server.headline());
                entry.incompatible = Some(server.clone());
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
            ConnEvent::Event(_) => {}
        }
        cx.emit(HostEvent { host, event });
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
