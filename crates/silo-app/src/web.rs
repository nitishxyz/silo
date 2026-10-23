//! The process-wide CEF engine: started with the first browser pane, pumped
//! from a gpui timer, shut down on quit. Remote hosts get one tunnel proxy
//! each, and a profile named after the host.

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

/// Polling period of the pump loop and the safety tick while pages exist.
const POLL: Duration = Duration::from_millis(4);
const TICK: Duration = Duration::from_millis(33);

/// Earliest pump CEF asked for (`on_schedule_message_pump_work`, any thread).
static DUE: Mutex<Option<Instant>> = Mutex::new(None);

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
        let mut last = Instant::now();
        loop {
            cx.background_executor().timer(POLL).await;
            let Some(runtime) = ENGINE.with_borrow(Clone::clone) else { return };
            let now = Instant::now();
            if !take_due(now) && !(Runtime::live_browsers() > 0 && now - last >= TICK) {
                continue;
            }
            last = now;
            runtime.pump();
            let dirty: Vec<WeakEntity<BrowserPane>> =
                LIVE.with_borrow(|live| live.iter().filter(|l| l.dirty.swap(false, Ordering::AcqRel)).filter_map(|l| l.pane.clone()).collect());
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

/// The loopback proxy of the remote host at `addr`, started once.
pub async fn tunnel(home: &Path, addr: silo_client::transport::EndpointAddr) -> anyhow::Result<silo_web::Tunnel> {
    let id = addr.id.to_string();
    let mut tunnels = TUNNELS.lock().await;
    let tunnels = tunnels.get_or_insert_with(HashMap::new);
    if let Some(proxy) = tunnels.get(&id) {
        return Ok(web_tunnel(proxy));
    }
    let net = crate::remote::net(home).await?;
    let proxy = silo_client::tunnel::TunnelProxy::start(net, addr).await?;
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
