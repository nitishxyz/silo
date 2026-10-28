//! The off-screen browser host (docs/AGENTS.md §4.1): browser blocks of this
//! Mac's server that no window shows, and the hidden pages of
//! `browser.fetch`. A window showing a hosted block adopts its pane (same
//! page, no reload) and hands it back when it stops showing it.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use gpui::{App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Subscription};
use serde_json::{Value, json};
use silo_client::Control;
use silo_proto::{
    agent::{BrowserFetchParams, BrowserFetchResult, BrowserScreenshotResult},
    control::{AppCall, BlockView, Event, Method, OwnMode, RpcError},
};
use silo_web::{Browser, DevToolsEvent};
use tokio::sync::Semaphore;

use crate::{
    browser_pane::{BrowserPane, Route},
    conn::ConnEvent,
    hosts_state::{HostEvent, HostsState},
    workspace::Workspace,
};

/// Fetch pages open at once; the rest wait their turn.
pub const FETCH_PAGES: usize = 3;
const FETCH_DEFAULT: Duration = Duration::from_secs(15);
const FETCH_MAX: Duration = Duration::from_secs(60);
/// Longest a fetch waits for a page (the server gives up at 120 s).
const QUEUE_MAX: Duration = Duration::from_secs(40);
/// How long the network must be quiet for a loaded page to count as settled.
const QUIET: Duration = Duration::from_millis(500);
/// Requests a settled page may keep in flight (long polls, beacons).
const QUIET_INFLIGHT: usize = 2;
/// How long a new page may take to answer CDP.
const READY: Duration = Duration::from_secs(10);
const FETCH_VIEW: silo_web::ViewSize = silo_web::ViewSize { width: 1280, height: 800, scale: 1.0 };
/// A bot check (Cloudflare's "Just a moment…") settles, then navigates on
/// its own: the page's title and whether it has a challenge form.
const CHALLENGE: &str =
    r"({title: document.title, form: !!document.querySelector('#challenge-form, #challenge-running, #challenge-stage, #cf-challenge-running')})";
const CHALLENGE_TITLES: &[&str] = &["just a moment", "attention required", "checking your browser", "please wait"];
/// How long a challenge may take to clear itself (automatic JS checks
/// take a few seconds) before the fetch reports it blocked.
const GRACE: Duration = Duration::from_secs(6);
/// How often a loaded page that hasn't settled is checked for a challenge.
const PROBE: Duration = Duration::from_secs(1);

/// Whether a page with `title` (and a challenge `form`) is a bot check.
pub(crate) fn challenged(title: &str, form: bool) -> bool {
    let title = title.trim().to_lowercase();
    form || CHALLENGE_TITLES.iter().any(|t| title.starts_with(t))
}

/// A bot check seen on the page, and since when.
#[derive(Debug, Default)]
pub(crate) struct Challenge {
    since: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// No challenge (or it cleared).
    Clear,
    /// A challenge, still within [`GRACE`].
    Wait,
    /// A challenge that hasn't cleared within [`GRACE`].
    Blocked,
}

impl Challenge {
    pub(crate) fn see(&mut self, challenged: bool, now: Instant) -> Gate {
        if !challenged {
            self.since = None;
            return Gate::Clear;
        }
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) >= GRACE { Gate::Blocked } else { Gate::Wait }
    }
}

/// The hosted panes by block: the adoption bookkeeping, apart from gpui.
pub(crate) struct Hosted<P> {
    panes: HashMap<String, P>,
}

impl<P> Default for Hosted<P> {
    fn default() -> Self {
        Self { panes: HashMap::new() }
    }
}

impl<P> Hosted<P> {
    /// A window shows `block`: it adopts the hosted pane.
    pub(crate) fn take(&mut self, block: &str) -> Option<P> {
        self.panes.remove(block)
    }

    /// No window shows `block` any more: its pane runs here.
    pub(crate) fn keep(&mut self, block: String, pane: P) {
        self.panes.insert(block, pane);
    }

    pub(crate) fn get(&self, block: &str) -> Option<&P> {
        self.panes.get(block)
    }

    /// Drop the panes `event` ends: blocks gone from their session's view,
    /// closed sessions, the server going away. `session` is a pane's block's
    /// session, once known.
    pub(crate) fn apply(&mut self, event: &ConnEvent, session: impl Fn(&P) -> Option<String>) {
        match event {
            ConnEvent::Event(Event::SessionChanged { view }) => {
                let alive: HashSet<&str> = view.blocks.iter().map(|b| b.id.as_str()).collect();
                self.panes.retain(|id, pane| alive.contains(id.as_str()) || session(pane).is_none_or(|s| s != view.session.id));
            }
            ConnEvent::Event(Event::SessionClosed { session_id }) => self.panes.retain(|_, pane| session(pane).is_none_or(|s| s != *session_id)),
            ConnEvent::Disconnected(_) | ConnEvent::NotPaired | ConnEvent::Incompatible | ConnEvent::IncompatibleServer(_) | ConnEvent::Quit => {
                self.panes.clear();
            }
            _ => {}
        }
    }

    /// Drop the copies of blocks a window shows with its own pane.
    pub(crate) fn retain_unshown(&mut self, shown: impl Fn(&str) -> bool) {
        self.panes.retain(|block, _| !shown(block));
    }
}

/// Who runs a forwarded `browser.*` request, as this app sees it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Runner {
    /// A window with the block's pane (`app_requests.rs`).
    Window,
    /// The host, for a block no window shows.
    Host(String),
    Fetch,
}

pub(crate) fn runner(call: &AppCall, shown: impl Fn(&str) -> bool) -> Option<Runner> {
    if call.method == "browser.fetch" {
        return Some(Runner::Fetch);
    }
    let block = call.params.get("block_id").and_then(Value::as_str).filter(|_| call.method.starts_with("browser."))?;
    Some(if shown(block) { Runner::Window } else { Runner::Host(block.to_owned()) })
}

pub struct BrowserHost {
    hosted: Hosted<Entity<BrowserPane>>,
    /// Remote servers that picked this app as their browser host: their hosted panes, by host index.
    remote: HashMap<usize, Hosted<Entity<BrowserPane>>>,
    slots: Rc<Semaphore>,
    _events: Subscription,
}

struct GlobalHost(Entity<BrowserHost>);

impl Global for GlobalHost {}

impl BrowserHost {
    pub fn install(hosts: &Entity<HostsState>, cx: &mut App) {
        let host = cx.new(|cx: &mut Context<Self>| Self {
            hosted: Hosted::default(),
            remote: HashMap::new(),
            slots: Rc::new(Semaphore::new(FETCH_PAGES)),
            _events: cx.subscribe(hosts, |this, _, event: &HostEvent, cx| this.host_event(event, cx)),
        });
        cx.set_global(GlobalHost(host));
    }

    fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalHost>().map(|g| g.0.clone())
    }

    /// A window shows `block` (of the remote host `remote`, else of this
    /// Mac's server): the hosted pane, if any.
    pub fn take(block: &str, remote: Option<usize>, cx: &mut App) -> Option<Entity<BrowserPane>> {
        Self::global(cx)?.update(cx, |this, _| match remote {
            Some(host) => this.remote.get_mut(&host)?.take(block),
            None => this.hosted.take(block),
        })
    }

    /// A new tab's pane made by the page that opened it (`browser_pane/tabs.rs`):
    /// a window already showing its block gets it in place of its own pane
    /// when `replace` (an adopted popup), else keeps its own; no window: it
    /// runs here until one shows it. False when it was not kept.
    pub fn adopt(pane: Entity<BrowserPane>, replace: bool, cx: &mut App) -> bool {
        let (block, route) = {
            let pane = pane.read(cx);
            (pane.block_id.clone(), pane.route().clone())
        };
        let remote = match &route {
            Route::Local => None,
            Route::Remote { host_id, .. } => {
                let hosts = HostsState::global(cx);
                let index = hosts.read(cx).entries().iter().position(|e| match &e.info.transport {
                    crate::hosts::Transport::Iroh { addr, .. } => addr.id.to_string() == *host_id,
                    crate::hosts::Transport::Shared { ticket, .. } => ticket.addr.id.to_string() == *host_id,
                    crate::hosts::Transport::Local { .. } => false,
                });
                let Some(index) = index else { return false };
                Some(index)
            }
        };
        let showing = cx.windows().into_iter().filter_map(|w| w.downcast::<Workspace>()).find(|w| {
            w.read(cx).ok().is_some_and(|w| {
                let host = match remote {
                    Some(index) => w.active_host == index,
                    None => w.hosts.get(w.active_host).is_some_and(|h| h.info.local),
                };
                host && w.blocks.browser(&block).is_some()
            })
        });
        if let Some(window) = showing {
            return replace && window.update(cx, |w, window, cx| w.install_browser(&block, pane, window, cx)).is_ok();
        }
        let Some(host) = Self::global(cx) else { return false };
        pane.update(cx, |pane, _| pane.detach());
        host.update(cx, |this, _| match remote {
            Some(index) => this.remote.entry(index).or_default().keep(block, pane),
            None => this.hosted.keep(block, pane),
        });
        true
    }

    /// A window stops showing `panes`: those of this Mac's server run on
    /// here. Returns the blocks of the others (theirs to disown).
    pub fn keep(panes: Vec<Entity<BrowserPane>>, cx: &mut App) -> Vec<String> {
        let host = Self::global(cx);
        let mut left = Vec::new();
        for pane in panes {
            let block = pane.read(cx).block_id.clone();
            match &host {
                Some(host) if pane.read(cx).is_local() => {
                    pane.update(cx, |pane, _| pane.detach());
                    host.update(cx, |this, _| this.hosted.keep(block, pane));
                }
                _ => left.push(block),
            }
        }
        if let Some(host) = host {
            // Another window may show the block with a pane of its own.
            let host = host.downgrade();
            cx.defer(move |cx| _ = host.update(cx, |this, cx| this.hosted.retain_unshown(|block| shown(block, cx))));
        }
        left
    }

    fn host_event(&mut self, event: &HostEvent, cx: &mut Context<Self>) {
        let hosts = HostsState::global(cx);
        let Some(entry) = hosts.read(cx).entries().get(event.host) else { return };
        if !entry.info.local {
            return self.remote_event(event, cx);
        }
        let control = entry.control.clone();
        self.hosted.apply(&event.event, |pane| pane.read(cx).info.as_ref().map(|i| i.session_id.clone()));
        match &event.event {
            ConnEvent::Event(Event::AppRequest { request_id, request, .. }) => {
                let Some(control) = control else { return };
                match runner(request, |block| shown(block, cx)) {
                    Some(Runner::Fetch) => self.fetch(control, Route::Local, request_id.clone(), request.params.clone(), cx),
                    Some(Runner::Host(block)) => self.run(control, None, Route::Local, block, request_id.clone(), request.clone(), cx),
                    Some(Runner::Window) | None => {}
                }
            }
            ConnEvent::Event(Event::BlockChanged { block }) => {
                if let Some(pane) = self.hosted.get(&block.id) {
                    pane.update(cx, |pane, cx| pane.set_info(block.clone(), cx));
                }
            }
            ConnEvent::Event(Event::BrowserCommand { block_id, command }) => {
                if let Some(pane) = self.hosted.get(block_id) {
                    pane.update(cx, |pane, cx| pane.apply_command(command, cx));
                }
            }
            _ => {}
        }
    }

    /// A paired remote server's event (it may pick this app as its browser
    /// host, `app_bridge::browser_host`): its fetches and unshown blocks run
    /// here in that host's profile, through its tunnel. A window showing such
    /// a block uses its own pane; the hosted copy goes. Shared links: nothing.
    fn remote_event(&mut self, event: &HostEvent, cx: &mut Context<Self>) {
        let hosts = HostsState::global(cx);
        let Some(entry) = hosts.read(cx).entries().get(event.host) else { return };
        let crate::hosts::Transport::Iroh { home, addr } = &entry.info.transport else { return };
        let route = Route::Remote { host_id: addr.id.to_string(), home: home.clone(), addr: addr.clone() };
        let (host, control) = (event.host, entry.control.clone());
        let on_host = |block: &str, cx: &App| {
            cx.windows()
                .iter()
                .filter_map(|w| w.downcast::<Workspace>())
                .filter_map(|w| w.read(cx).ok())
                .any(|w| w.active_host == host && w.blocks.browser(block).is_some())
        };
        if let Some(hosted) = self.remote.get_mut(&host) {
            hosted.apply(&event.event, |pane| pane.read(cx).info.as_ref().map(|i| i.session_id.clone()));
            hosted.retain_unshown(|block| on_host(block, cx));
            match &event.event {
                ConnEvent::Event(Event::BlockChanged { block }) => {
                    if let Some(pane) = hosted.get(&block.id) {
                        pane.update(cx, |pane, cx| pane.set_info(block.clone(), cx));
                    }
                }
                ConnEvent::Event(Event::BrowserCommand { block_id, command }) => {
                    if let Some(pane) = hosted.get(block_id) {
                        pane.update(cx, |pane, cx| pane.apply_command(command, cx));
                    }
                }
                _ => {}
            }
        }
        if let (ConnEvent::Event(Event::AppRequest { request_id, request, .. }), Some(control)) = (&event.event, control) {
            match runner(request, |block| on_host(block, cx)) {
                Some(Runner::Fetch) => self.fetch(control, route, request_id.clone(), request.params.clone(), cx),
                Some(Runner::Host(block)) => self.run(control, Some(host), route, block, request_id.clone(), request.clone(), cx),
                Some(Runner::Window) | None => {}
            }
        }
    }

    /// A browser action on `block`, which no window shows: run its page
    /// here (opening it first), claim it, execute. `remote`: the remote
    /// host it belongs to, its page opened on `route`.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        control: Control,
        remote: Option<usize>,
        route: Route,
        block: String,
        request_id: String,
        call: AppCall,
        cx: &mut Context<Self>,
    ) {
        let (rt, home) = {
            let hosts = HostsState::global(cx);
            let hosts = hosts.read(cx);
            (hosts.rt.clone(), hosts.home.clone())
        };
        let hosted = match remote {
            Some(host) => self.remote.entry(host).or_default(),
            None => &mut self.hosted,
        };
        let fresh = hosted.get(&block).is_none();
        if fresh {
            let pane = cx.new(|cx| BrowserPane::new(block.clone(), rt.clone(), Some(control.clone()), route, home, cx));
            pane.update(cx, |pane, _| pane.detach());
            hosted.keep(block.clone(), pane);
        }
        let Some(pane) = hosted.get(&block).cloned() else { return };
        pane.update(cx, |pane, cx| pane.open_offscreen(cx));
        let timeout = call.params.get("timeout_ms").and_then(Value::as_u64).map_or(30_000, |ms| ms.saturating_add(5_000)).min(120_000);
        let host = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result = async {
                if fresh {
                    let (c, b) = (control.clone(), block.clone());
                    match rt.spawn(async move { c.call::<BlockView>(Method::BlockView { block_id: b }).await }).await? {
                        Ok(view) => pane.update(cx, |pane, cx| pane.set_info(view.block, cx)),
                        Err(error) => {
                            _ = host.update(cx, |this, _| match remote {
                                Some(r) => this.remote.get_mut(&r).and_then(|h| h.take(&block)),
                                None => this.hosted.take(&block),
                            });
                            return Err(error);
                        }
                    }
                }
                let (c, b) = (control.clone(), block.clone());
                rt.spawn(async move { c.call_value(Method::OwnBlock { block_ids: vec![b], mode: OwnMode::Request }).await }).await??;
                let begun = Instant::now();
                while !pane.read_with(cx, |pane, _| pane.page_ready()) {
                    if let Some(error) = pane.read_with(cx, |pane, _| pane.page_error().map(str::to_owned)) {
                        anyhow::bail!("the page could not open: {error}");
                    }
                    if begun.elapsed() > READY {
                        anyhow::bail!("browser is not ready");
                    }
                    cx.background_executor().timer(Duration::from_millis(50)).await;
                }
                let executor = pane.update(cx, |pane, cx| pane.executor(cx))?;
                tokio::select! {
                    result = executor.run(&call.method, call.params) => result,
                    () = cx.background_executor().timer(Duration::from_millis(timeout)) => Err(anyhow::anyhow!("browser request timed out")),
                }
            }
            .await;
            reply(&rt, control, request_id, result.map_err(|e| e.downcast::<RpcError>().unwrap_or_else(|e| RpcError::bad_request(format!("{e:#}")))));
        })
        .detach();
    }

    /// `browser.fetch`: a hidden page in the agent profile (a remote
    /// server's: its host profile, through its tunnel), at most
    /// [`FETCH_PAGES`] at once.
    fn fetch(&mut self, control: Control, route: Route, request_id: String, params: Value, cx: &mut Context<Self>) {
        let (rt, home) = {
            let hosts = HostsState::global(cx);
            let hosts = hosts.read(cx);
            (hosts.rt.clone(), hosts.home.clone())
        };
        let slots = self.slots.clone();
        cx.spawn(async move |_, cx| {
            let timers = cx.background_executor().clone();
            let result = async {
                let mut params: BrowserFetchParams = serde_json::from_value(params).map_err(|e| RpcError::bad_request(e.to_string()))?;
                params.url = web_url(&params.url).ok_or_else(|| RpcError::bad_request("url must be an http or https URL"))?;
                let _slot = tokio::select! {
                    slot = slots.acquire() => slot.map_err(RpcError::internal)?,
                    () = timers.timer(QUEUE_MAX) => return Err(RpcError::new(RpcError::TIMEOUT, "too many pages are loading; try again")),
                };
                let (profile, tunnel) = match route {
                    Route::Local => (silo_web::AGENT.to_owned(), None),
                    Route::Remote { host_id, home, addr } => {
                        let tunnel = rt.spawn(async move { crate::web::tunnel(&home, addr).await }).await.map_err(RpcError::internal)?;
                        (host_id, Some(tunnel.map_err(|e| RpcError::internal(format!("{e:#}")))?))
                    }
                };
                let page = cx.update(|cx| open_page(&home, &profile, tunnel, cx)).map_err(|e| RpcError::internal(format!("{e:#}")))?;
                let result = load(&page, &params, &timers).await;
                crate::web::close(&page);
                result
            }
            .await;
            reply(&rt, control, request_id, result.and_then(|r| serde_json::to_value(r).map_err(RpcError::internal)));
        })
        .detach();
    }
}

/// Whether a window of this Mac's server has a pane for `block`.
fn shown(block: &str, cx: &App) -> bool {
    cx.windows()
        .iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .filter_map(|w| w.read(cx).ok())
        .any(|w| w.hosts.get(w.active_host).is_some_and(|h| h.info.local) && w.blocks.browser(block).is_some())
}

fn reply(rt: &tokio::runtime::Handle, control: Control, request_id: String, result: Result<Value, RpcError>) {
    let (result, error) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    rt.spawn(async move {
        let _ = control.call_value(Method::AppReply { request_id, result, error }).await;
    });
}

/// `url` if it is http(s).
fn web_url(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    (matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") && !rest.is_empty()).then(|| url.to_owned())
}

/// A hidden page in `profile` (a remote host's with its tunnel), blank until navigated.
fn open_page(home: &Path, profile: &str, tunnel: Option<silo_web::Tunnel>, cx: &mut App) -> anyhow::Result<Rc<Browser>> {
    let runtime = crate::web::runtime(home, cx)?;
    let profile = runtime.profile(profile, tunnel)?;
    let options = silo_web::BrowserOptions { url: "about:blank", profile: &profile, size: FETCH_VIEW, offscreen: true, notify: Arc::new(|| {}) };
    let page = Rc::new(runtime.create_browser(options)?);
    crate::web::register(page.clone(), Arc::new(AtomicBool::new(false)), None);
    Ok(page)
}

/// A page's load, followed through its CDP events.
#[derive(Debug, Default)]
pub(crate) struct Load {
    main: Option<String>,
    /// The main document's request and its HTTP status.
    document: Option<String>,
    pub(crate) status: Option<u16>,
    /// The load event fired for the current document.
    loaded: bool,
    inflight: HashSet<String>,
    /// The last request started or ended.
    active: Option<Instant>,
}

impl Load {
    pub(crate) fn new(main: Option<String>) -> Self {
        Self { main, ..Self::default() }
    }

    pub(crate) fn feed(&mut self, event: &DevToolsEvent, now: Instant) {
        let p = &event.params;
        let id = p["requestId"].as_str();
        match event.method.as_str() {
            "Page.frameNavigated" if p["frame"].get("parentId").is_none() => {
                self.main = p["frame"]["id"].as_str().map(str::to_owned).or(self.main.take());
                self.loaded = false;
            }
            "Page.loadEventFired" => self.loaded = true,
            "Network.requestWillBeSent" => {
                let Some(id) = id else { return };
                if p["type"] == "Document" && (self.main.is_none() || p["frameId"].as_str() == self.main.as_deref()) {
                    self.document = Some(id.to_owned());
                    self.status = None;
                }
                self.inflight.insert(id.to_owned());
                self.active = Some(now);
            }
            "Network.responseReceived" if id.is_some() && id == self.document.as_deref() => {
                self.status = p["response"]["status"].as_u64().and_then(|s| u16::try_from(s).ok());
            }
            "Network.loadingFinished" | "Network.loadingFailed" if id.is_some_and(|id| self.inflight.remove(id)) => self.active = Some(now),
            _ => {}
        }
    }

    /// Loaded, and the network quiet for [`QUIET`].
    pub(crate) fn settled(&self, now: Instant) -> bool {
        self.loaded && self.inflight.len() <= QUIET_INFLIGHT && self.active.is_none_or(|t| now.duration_since(t) >= QUIET)
    }

    /// When [`Self::settled`] may next turn true without an event.
    pub(crate) fn wake(&self, now: Instant) -> Instant {
        self.active.map_or(now, |t| t + QUIET).max(now + Duration::from_millis(10))
    }

    /// Not settled yet: wait another quiet period.
    pub(crate) fn busy(&mut self, now: Instant) {
        self.active = Some(now);
    }
}

async fn cdp(page: &Browser, timers: &BackgroundExecutor, method: &str, params: Value, limit: Duration) -> Result<Value, RpcError> {
    tokio::select! {
        result = page.cdp(method, params) => result.map_err(|e| RpcError::internal(format!("{method}: {e:#}"))),
        () = timers.timer(limit) => Err(RpcError::new(RpcError::TIMEOUT, format!("the page did not answer {method}"))),
    }
}

/// Load `params.url` in `page`, wait for it to settle (or the deadline),
/// then read it.
async fn load(page: &Browser, params: &BrowserFetchParams, timers: &BackgroundExecutor) -> Result<BrowserFetchResult, RpcError> {
    const ANSWER: Duration = Duration::from_secs(5);
    let begun = Instant::now();
    while !page.ready() {
        if begun.elapsed() > READY {
            return Err(RpcError::internal("the browser engine did not open a page"));
        }
        timers.timer(Duration::from_millis(20)).await;
    }
    let mut events = page.subscribe_cdp();
    for domain in ["Page", "Network"] {
        cdp(page, timers, &format!("{domain}.enable"), json!({}), ANSWER).await?;
    }
    let tree = cdp(page, timers, "Page.getFrameTree", json!({}), ANSWER).await?;
    let mut state = Load::new(tree["frameTree"]["frame"]["id"].as_str().map(str::to_owned));
    let wait = params.timeout_ms.map_or(FETCH_DEFAULT, Duration::from_millis).min(FETCH_MAX);
    let deadline = Instant::now() + wait;
    let navigated = cdp(page, timers, "Page.navigate", json!({"url": params.url}), wait).await;
    let timeout = || RpcError::new(RpcError::TIMEOUT, format!("{} did not load within {} s", params.url, wait.as_secs()));
    match navigated {
        Err(error) if error.code == RpcError::TIMEOUT => return Err(timeout()),
        Err(error) => return Err(error),
        Ok(result) => {
            // An HTTP error status with no body still renders (and has its status).
            if let Some(error) = result["errorText"].as_str().filter(|e| !e.is_empty() && *e != "net::ERR_HTTP_RESPONSE_CODE_FAILURE") {
                return Err(RpcError::new(RpcError::NAVIGATION, format!("{} could not load: {error}", params.url)));
            }
        }
    }
    let mut timed_out = false;
    let (mut challenge, mut probed, mut blocked) = (Challenge::default(), None::<Instant>, false);
    loop {
        let now = Instant::now();
        if now >= deadline {
            timed_out = true;
            break;
        }
        let settled = state.settled(now);
        // A challenge page may never settle (its widget polls): look anyway once loaded.
        if settled || (state.loaded && probed.is_none_or(|t: Instant| now.duration_since(t) >= PROBE)) {
            probed = Some(now);
            let read = cdp(page, timers, "Runtime.evaluate", json!({"expression": CHALLENGE, "returnByValue": true}), ANSWER).await?;
            let value = &read["result"]["value"];
            match challenge.see(challenged(value["title"].as_str().unwrap_or_default(), value["form"] == json!(true)), Instant::now()) {
                Gate::Blocked => {
                    blocked = true;
                    break;
                }
                Gate::Clear if settled => break,
                Gate::Wait if settled => state.busy(Instant::now()),
                Gate::Clear | Gate::Wait => {}
            }
        }
        let wake = state.wake(now).min(deadline);
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if event.method == "Page.javascriptDialogOpening" {
                        page.answer_dialog(false, None);
                    }
                    state.feed(&event, Instant::now());
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err(RpcError::internal("the page closed")),
            },
            () = timers.timer(wake.saturating_duration_since(now)) => {}
        }
    }
    let selector = serde_json::to_string(&params.selector).map_err(RpcError::internal)?;
    let expression = format!(
        "(() => {{ const s = {selector}; const el = s === null ? document.documentElement : document.querySelector(s); \
         return {{url: location.href, title: document.title, html: el ? el.outerHTML : null}}; }})()"
    );
    let read = cdp(page, timers, "Runtime.evaluate", json!({"expression": expression, "returnByValue": true}), ANSWER).await?;
    if let Some(exception) = read.get("exceptionDetails") {
        let text = exception["exception"]["description"].as_str().or(exception["text"].as_str()).unwrap_or("error");
        return Err(RpcError::bad_request(format!("selector: {text}")));
    }
    let value = &read["result"]["value"];
    let url = value["url"].as_str().unwrap_or_default().to_owned();
    if timed_out && silo_web::is_blank(&url) {
        return Err(timeout());
    }
    let Some(html) = value["html"].as_str() else {
        return Err(RpcError::bad_request(format!("selector {} matched nothing", params.selector.as_deref().unwrap_or_default())));
    };
    let (html, truncated) = cut(html, BrowserFetchResult::HTML_MAX);
    let screenshot = if params.screenshot == Some(true) { screenshot(page, timers).await } else { None };
    Ok(BrowserFetchResult {
        url,
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        status: state.status,
        html,
        timed_out,
        blocked,
        truncated,
        screenshot,
    })
}

/// The viewport, as png (jpeg when the png passes 4 MiB).
async fn screenshot(page: &Browser, timers: &BackgroundExecutor) -> Option<BrowserScreenshotResult> {
    // The page paints a few times a second off screen: ask for a frame now.
    page.invalidate();
    for format in ["png", "jpeg"] {
        let params = if format == "png" { json!({"format": format}) } else { json!({"format": format, "quality": 80}) };
        let shot = match cdp(page, timers, "Page.captureScreenshot", params, Duration::from_secs(10)).await {
            Ok(shot) => shot,
            Err(error) => {
                tracing::warn!("browser.fetch screenshot: {}", error.message);
                return None;
            }
        };
        let data = shot["data"].as_str()?;
        if data.len() > 4 * 1024 * 1024 {
            continue;
        }
        let bytes = data_encoding::BASE64.decode(data.as_bytes()).ok()?;
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()?;
        return Some(BrowserScreenshotResult { mime: format!("image/{format}"), data_base64: data.to_owned(), width, height });
    }
    None
}

/// `text` cut to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{SessionView, TermSize};

    use super::*;

    fn event(method: &str, params: Value) -> DevToolsEvent {
        DevToolsEvent { method: method.into(), params }
    }

    #[test]
    fn hosted_panes_are_adopted_and_handed_back_once() {
        let mut hosted = Hosted::default();
        hosted.keep("b1".into(), "page one");
        assert_eq!(hosted.take("b1"), Some("page one"), "the window adopts the same page");
        assert_eq!(hosted.take("b1"), None, "adopted once");
        hosted.keep("b1".into(), "page one");
        hosted.keep("b2".into(), "page two");
        hosted.retain_unshown(|block| block == "b2");
        assert_eq!(hosted.get("b2"), None, "a window shows b2 with its own pane");
        assert_eq!(hosted.get("b1"), Some(&"page one"));
    }

    fn view(session: &str, blocks: &[&str]) -> ConnEvent {
        let size = serde_json::to_value(TermSize::new(80, 24)).unwrap();
        let blocks: Vec<Value> =
            blocks.iter().map(|id| json!({"id": id, "session_id": session, "kind": "browser", "title": "", "size": size})).collect();
        let session = json!({"id": session, "name": session, "windows": [], "active_window": "", "revision": 1, "created_at": 0});
        let view: SessionView = serde_json::from_value(json!({"session": session, "blocks": blocks})).unwrap();
        ConnEvent::Event(Event::SessionChanged { view })
    }

    #[test]
    fn a_hosted_page_lives_as_long_as_its_block() {
        let mut hosted = Hosted::default();
        for (block, session) in [("b1", "s1"), ("b2", "s1"), ("b3", "s2"), ("b4", "")] {
            hosted.keep(block.into(), session);
        }
        let session = |s: &&str| Some(s.to_string()).filter(|s| !s.is_empty());
        hosted.apply(&view("s1", &["b1"]), session);
        assert!(hosted.get("b1").is_some() && hosted.get("b2").is_none(), "b2 closed");
        assert!(hosted.get("b3").is_some() && hosted.get("b4").is_some(), "other sessions, and a block still opening, stay");
        hosted.apply(&ConnEvent::Event(Event::SessionClosed { session_id: "s2".into() }), session);
        assert!(hosted.get("b3").is_none() && hosted.get("b1").is_some());
        hosted.apply(&ConnEvent::Disconnected("gone".into()), session);
        assert!(hosted.get("b1").is_none() && hosted.get("b4").is_none(), "the server's blocks went with it");
    }

    #[test]
    fn requests_go_to_a_window_showing_the_block_else_the_host() {
        let call = |method: &str, params: Value| AppCall { method: method.into(), params };
        let shown = |block: &str| block == "b1";
        assert_eq!(runner(&call("browser.snapshot", json!({"block_id": "b1"})), shown), Some(Runner::Window));
        assert_eq!(runner(&call("browser.click", json!({"block_id": "b2", "target": "e1"})), shown), Some(Runner::Host("b2".into())));
        assert_eq!(runner(&call("browser.fetch", json!({"url": "https://example.com"})), shown), Some(Runner::Fetch));
        assert_eq!(runner(&call("app.copy", json!({"block_id": "b2"})), shown), None);
        assert_eq!(runner(&call("browser.snapshot", json!({})), shown), None);
    }

    #[test]
    fn a_page_settles_after_load_and_network_quiet() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut load = Load::new(Some("main".into()));
        let request =
            |id: &str, kind: &str, frame: &str| event("Network.requestWillBeSent", json!({"requestId": id, "type": kind, "frameId": frame}));
        load.feed(&request("doc", "Document", "main"), at(0));
        load.feed(&event("Network.responseReceived", json!({"requestId": "doc", "response": {"status": 503}})), at(10));
        load.feed(&event("Page.frameNavigated", json!({"frame": {"id": "main"}})), at(20));
        load.feed(&request("ad", "Document", "child"), at(30));
        load.feed(&event("Network.responseReceived", json!({"requestId": "ad", "response": {"status": 404}})), at(40));
        assert_eq!(load.status, Some(503), "an iframe's document is not the page's");
        for id in ["a", "b", "c"] {
            load.feed(&request(id, "Script", "main"), at(50));
        }
        load.feed(&event("Page.loadEventFired", json!({})), at(60));
        assert!(!load.settled(at(2000)), "five requests in flight");
        for id in ["doc", "ad", "a"] {
            load.feed(&event("Network.loadingFinished", json!({"requestId": id})), at(100));
        }
        assert!(!load.settled(at(599)), "quiet for less than 500 ms");
        assert_eq!(load.wake(at(150)), at(600));
        assert!(load.settled(at(600)), "two long polls may stay in flight");
        load.busy(at(600));
        assert!(!load.settled(at(700)), "a challenge page keeps waiting");
        // The challenge navigates to the real page: a new document.
        load.feed(&request("real", "Document", "main"), at(800));
        load.feed(&event("Network.responseReceived", json!({"requestId": "real", "response": {"status": 200}})), at(810));
        load.feed(&event("Page.frameNavigated", json!({"frame": {"id": "main"}})), at(820));
        load.feed(&event("Network.loadingFinished", json!({"requestId": "real"})), at(830));
        assert!(!load.settled(at(2000)), "not loaded yet");
        load.feed(&event("Page.loadEventFired", json!({})), at(840));
        assert!(load.settled(at(1330)));
        assert_eq!(load.status, Some(200));
    }

    #[test]
    fn bot_checks_are_told_from_pages() {
        assert!(challenged("Just a moment...", false));
        assert!(challenged("  Attention Required! | Cloudflare", false));
        assert!(challenged("", true), "Cloudflare's challenge form, whatever the title");
        assert!(!challenged("Example Domain", false));
        assert!(!challenged("How to wait: just a moment", false), "only a title that starts with it");
    }

    #[test]
    fn a_challenge_that_does_not_clear_is_blocked_after_a_grace_period() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut challenge = Challenge::default();
        assert_eq!(challenge.see(false, at(0)), Gate::Clear);
        assert_eq!(challenge.see(true, at(1000)), Gate::Wait, "an automatic check gets time to clear");
        assert_eq!(challenge.see(true, at(6999)), Gate::Wait);
        assert_eq!(challenge.see(false, at(7000)), Gate::Clear, "it cleared: the real page");
        assert_eq!(challenge.see(true, at(8000)), Gate::Wait, "a new challenge starts its own grace");
        assert_eq!(challenge.see(true, at(13_999)), Gate::Wait);
        assert_eq!(challenge.see(true, at(14_000)), Gate::Blocked, "6 s without clearing");
    }

    #[tokio::test]
    async fn fetch_pages_are_limited_and_queued() {
        let slots = Rc::new(Semaphore::new(FETCH_PAGES));
        let held: Vec<_> = (0..FETCH_PAGES).filter_map(|_| slots.try_acquire().ok()).collect();
        assert_eq!(held.len(), FETCH_PAGES);
        assert!(slots.try_acquire().is_err(), "three pages at once");
        let mut waiting = std::pin::pin!(slots.acquire());
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting.as_mut()).await.is_err(), "the fourth waits");
        drop(held);
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting).await.is_ok(), "and runs once one closes");
    }

    #[test]
    fn fetch_urls_and_html_caps() {
        assert_eq!(web_url(" https://a.b/c ").as_deref(), Some("https://a.b/c"));
        assert_eq!(web_url("HTTP://a.b").as_deref(), Some("HTTP://a.b"));
        for bad in ["a.b", "file:///etc/hosts", "https://", "about:blank", "javascript://x"] {
            assert_eq!(web_url(bad), None, "{bad}");
        }
        assert_eq!(cut("héllo", 2), ("h".into(), true));
        assert_eq!(cut("hi", 2), ("hi".into(), false));
    }
}
