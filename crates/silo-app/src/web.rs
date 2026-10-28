//! The process-wide CEF engine: started with the first browser pane, pumped
//! as soon as CEF asks (plus a safety tick), shut down on quit. Pages paint
//! when asked for a frame: a shown pane asks on each of its window's
//! refreshes ([`Frames`]), the tick asks for the rest. Remote hosts get one
//! tunnel proxy each, and a profile named after the host.

use std::{
    cell::RefCell,
    collections::HashMap,
    os::unix::fs::{DirBuilderExt as _, PermissionsExt as _},
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{App, WeakEntity};
use silo_web::{Browser, Config, Runtime};

use crate::browser_pane::BrowserPane;

/// The safety tick while pages exist: CEF never re-announces delayed work
/// it saw during a pump, and pages nobody drives get frames on it.
const TICK: Duration = Duration::from_millis(33);

/// Earliest pump CEF asked for (`on_schedule_message_pump_work`, any thread).
static DUE: Mutex<Option<Instant>> = Mutex::new(None);
/// Wakes the pump loop when CEF asks for work.
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

struct Live {
    browser: Rc<Browser>,
    dirty: Arc<AtomicBool>,
    /// `None`: a fetch page (`browser_host.rs`), read through CDP only.
    pane: Option<WeakEntity<BrowserPane>>,
}

thread_local! {
    static ENGINE: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) };
    static LIVE: RefCell<Vec<Live>> = const { RefCell::new(Vec::new()) };
}

fn schedule(delay: Duration) {
    let at = Instant::now() + delay;
    let mut due = DUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if due.is_none_or(|d| at < d) {
        *due = Some(at);
        drop(due);
        WAKE.notify_one();
    }
}

/// When the pump loop next has to pump: CEF's due time, or the tick after
/// `last` while pages exist; `None`: only when CEF asks.
fn next_pump(due: Option<Instant>, last: Instant, pages: bool) -> Option<Instant> {
    let tick = pages.then_some(last + TICK);
    match (due, tick) {
        (Some(due), Some(tick)) => Some(due.min(tick)),
        (due, tick) => due.or(tick),
    }
}

fn take_due(now: Instant) -> bool {
    let mut due = DUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let ready = due.is_some_and(|d| d <= now);
    if ready {
        *due = None;
    }
    ready
}

/// The engine, initialized on first use (engine data under `<home>/web`).
pub fn runtime(home: &Path, cx: &mut App) -> anyhow::Result<Rc<Runtime>> {
    if let Some(runtime) = ENGINE.with_borrow(Clone::clone) {
        return Ok(runtime);
    }
    // No keychain: Chromium would prompt for "Chromium Safe Storage". Profile
    // data is protected by its 0700 directory instead.
    let switches = vec!["use-mock-keychain".to_owned()];
    let root = home.join("web");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let config = Config { root, schedule: Arc::new(schedule), switches };
    let runtime = Rc::new(Runtime::init(config)?);
    tracing::info!("browser engine started");
    ENGINE.set(Some(runtime.clone()));
    cx.spawn(async move |cx| {
        let (mut last, mut driven) = (Instant::now(), Instant::now());
        loop {
            let due = *DUE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            match next_pump(due, last, Runtime::live_browsers() > 0) {
                Some(at) if at <= now => {}
                Some(at) => {
                    let timer = cx.background_executor().timer(at - now);
                    tokio::select! {
                        () = WAKE.notified() => {}
                        () = timer => {}
                    }
                    continue;
                }
                None => {
                    WAKE.notified().await;
                    continue;
                }
            }
            let Some(runtime) = ENGINE.with_borrow(Clone::clone) else { return };
            take_due(now);
            last = now;
            runtime.pump();
            // On the tick only: shown pages get theirs on their window's refreshes.
            if now - driven >= TICK {
                driven = now;
                runtime.drive_pages();
            }
            let dirty: Vec<WeakEntity<BrowserPane>> =
                LIVE.with_borrow(|live| live.iter().filter(|l| l.dirty.swap(false, Ordering::AcqRel)).filter_map(|l| l.pane.clone()).collect());
            if dirty.is_empty() {
                continue;
            }
            cx.update(|cx| {
                for pane in dirty {
                    let _ = pane.update(cx, |pane, cx| pane.engine_changed(cx));
                }
            });
        }
    })
    .detach();
    Ok(runtime)
}

/// Track a page so quitting can close it; `dirty` is set by its notify.
pub fn register(browser: Rc<Browser>, dirty: Arc<AtomicBool>, pane: Option<WeakEntity<BrowserPane>>) {
    LIVE.with_borrow_mut(|live| live.push(Live { browser, dirty, pane }));
}

/// Close a page (its pane went away).
pub fn close(browser: &Rc<Browser>) {
    browser.close();
    LIVE.with_borrow_mut(|live| live.retain(|l| !Rc::ptr_eq(&l.browser, browser)));
}

/// On quit: close every page, then shut the engine down.
pub fn shutdown() {
    for live in LIVE.take() {
        live.browser.close();
    }
    let Some(runtime) = ENGINE.take() else { return };
    match Rc::try_unwrap(runtime) {
        Ok(runtime) => {
            if let Err(error) = runtime.shutdown(Duration::from_secs(3)) {
                tracing::warn!("browser engine: {error:#}");
            }
        }
        Err(_) => tracing::warn!("browser engine still in use at quit"),
    }
}

static TUNNELS: tokio::sync::Mutex<Option<HashMap<String, silo_client::tunnel::TunnelProxy>>> = tokio::sync::Mutex::const_new(None);

/// The loopback proxy of the remote host at `addr`, started once on the
/// host's shared tunnel dialer (`agent::client::dialer`).
pub async fn tunnel(home: &Path, addr: silo_client::transport::EndpointAddr) -> anyhow::Result<silo_web::Tunnel> {
    let id = addr.id.to_string();
    let mut tunnels = TUNNELS.lock().await;
    let tunnels = tunnels.get_or_insert_with(HashMap::new);
    if let Some(proxy) = tunnels.get(&id) {
        return Ok(web_tunnel(proxy));
    }
    crate::agent::client::ensure_route(&id, home.to_owned(), addr);
    let proxy = silo_client::tunnel::TunnelProxy::with_dialer(crate::agent::client::dialer(&id).await?).await?;
    tracing::info!(host = %id, port = proxy.port(), "browser tunnel");
    let tunnel = web_tunnel(&proxy);
    tunnels.insert(id, proxy);
    Ok(tunnel)
}

fn web_tunnel(proxy: &silo_client::tunnel::TunnelProxy) -> silo_web::Tunnel {
    let (user, pass) = proxy.credentials();
    silo_web::Tunnel { port: proxy.port(), user: user.to_owned(), pass: pass.to_owned() }
}

/// Stop the proxy of a removed host (`id`: its EndpointId).
pub async fn close_tunnel(id: String) {
    if let Some(tunnels) = TUNNELS.lock().await.as_mut() {
        tunnels.remove(&id);
    }
}

/// How long a page keeps a frame on every refresh after it painted or got input.
const ACTIVE: Duration = Duration::from_secs(1);

/// Paces a shown page's frames by its window's refreshes (`W`: the window):
/// one each while it paints or gets input, none once it's idle (the pump's
/// tick still asks now and then, so a change shows and wakes it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frames<W> {
    /// The window whose refreshes drive the page.
    window: Option<W>,
    active_until: Option<Instant>,
}

impl<W> Default for Frames<W> {
    fn default() -> Self {
        Self { window: None, active_until: None }
    }
}

impl<W: Copy + PartialEq> Frames<W> {
    /// A paint or input: frames on every refresh for a while. True when no
    /// refresh drives the page yet: ask for one frame now.
    pub fn wake(&mut self, now: Instant) -> bool {
        self.active_until = Some(now + ACTIVE);
        self.window.is_none()
    }

    fn active(&self, now: Instant) -> bool {
        self.active_until.is_some_and(|until| now < until)
    }

    /// The page painted in `window`: whether to start driving it from there.
    pub fn start(&mut self, window: W, shown: bool, now: Instant) -> bool {
        let start = shown && self.active(now) && self.window != Some(window);
        if start {
            self.window = Some(window);
        }
        start
    }

    /// A refresh of `window`: whether to ask for a frame (and keep going).
    pub fn refresh(&mut self, window: W, shown: bool, now: Instant) -> bool {
        if self.window != Some(window) {
            return false;
        }
        let go = shown && self.active(now);
        if !go {
            self.window = None;
        }
        go
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pump_waits_for_cef_or_the_tick() {
        let now = Instant::now();
        assert_eq!(next_pump(None, now, false), None, "no pages: only when CEF asks");
        assert_eq!(next_pump(None, now, true), Some(now + TICK));
        let soon = now + Duration::from_millis(2);
        assert_eq!(next_pump(Some(soon), now, true), Some(soon));
        assert_eq!(next_pump(Some(now + TICK * 2), now, true), Some(now + TICK));
        assert_eq!(next_pump(Some(soon), now, false), Some(soon));
    }

    #[test]
    fn frames_follow_refreshes_while_the_page_is_active() {
        let now = Instant::now();
        let mut frames = Frames::default();
        assert!(!frames.start(1, true, now), "idle: no refresh loop");
        assert!(frames.wake(now), "input with no loop: ask a frame now");
        assert!(frames.start(1, true, now));
        assert!(!frames.start(1, true, now), "already driven from that window");
        assert!(!frames.wake(now), "driven: the next refresh asks");
        assert!(frames.refresh(1, true, now + Duration::from_millis(8)));
        assert!(!frames.refresh(2, true, now), "another window's stale loop stops");
        assert!(frames.start(2, true, now), "moved to another window");
        assert!(!frames.refresh(1, true, now), "the old window's loop stops");
        assert!(!frames.refresh(2, true, now + ACTIVE), "idle: stops");
        assert!(!frames.refresh(2, true, now + ACTIVE), "and stays stopped");
        assert!(frames.wake(now + ACTIVE), "woken again with no loop");
        assert!(frames.start(2, true, now + ACTIVE));
        assert!(!frames.refresh(2, false, now + ACTIVE), "hidden: stops");
        assert!(!frames.start(2, false, now + ACTIVE), "hidden: never starts");
    }
}
