//! Process-wide CEF engine: framework load, NSApp protocol, init, pump, shutdown.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cef::rc::Rc as _;
use cef::*;

use crate::layout::Layout;

static LIVE_BROWSERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn browser_opened() {
    LIVE_BROWSERS.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn browser_closed() {
    LIVE_BROWSERS.fetch_sub(1, Ordering::SeqCst);
}

thread_local! {
    /// Every page's state, for [`Runtime::drive_pages`] (CEF's UI thread).
    static PAGES: std::cell::RefCell<Vec<std::sync::Weak<crate::shared::Shared>>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn track(shared: &Arc<crate::shared::Shared>) {
    PAGES.with_borrow_mut(|pages| {
        pages.retain(|page| page.strong_count() > 0);
        pages.push(Arc::downgrade(shared));
    });
}

/// Whether a page last asked for a frame at `asked` is due one at `now`,
/// at `fps` frames a second.
fn frame_due(asked: Option<Instant>, fps: u32, now: Instant) -> bool {
    asked.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1) / fps.max(1))
}

/// Called (from any thread) when CEF wants `Runtime::pump` after `delay`.
pub type Schedule = Arc<dyn Fn(Duration) + Send + Sync>;

wrap_browser_process_handler! {
    struct PumpHandler {
        schedule: Schedule,
    }

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            (self.schedule)(Duration::from_millis(delay_ms.max(0) as u64));
        }
    }
}

wrap_app! {
    struct BrowserApp {
        handler: BrowserProcessHandler,
        switches: Vec<String>,
    }

    impl App {
        fn on_before_command_line_processing(&self, _process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            let Some(command_line) = command_line else { return };
            // Route every auth challenge to `RequestHandler::auth_credentials` instead of Chrome's
            // login dialog (invisible in windowless mode, so loads would hang): tunnel proxies are
            // answered there, everything else is cancelled.
            command_line.append_switch(Some(&CefString::from("disable-chrome-login-prompt")));
            for switch in &self.switches {
                match switch.split_once('=') {
                    Some((name, value)) => command_line.append_switch_with_value(
                        Some(&CefString::from(name)),
                        Some(&CefString::from(value)),
                    ),
                    None => command_line.append_switch(Some(&CefString::from(switch.as_str()))),
                }
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(self.handler.clone())
        }
    }
}

pub struct Config {
    /// Engine data dir: `<root>/cef.log`, profiles in `<root>/profiles/<name>` (`root_cache_path`).
    pub root: PathBuf,
    /// Asked to call `Runtime::pump` after the given delay (external message pump).
    pub schedule: Schedule,
    /// Extra Chromium switches, `name` or `name=value`, e.g. `"use-mock-keychain"` for dev runs.
    pub switches: Vec<String>,
}

/// The CEF engine. One per process, main thread only.
pub struct Runtime {
    profiles_dir: PathBuf,
    profiles: std::cell::RefCell<std::collections::HashMap<String, crate::Profile>>,
    _args: args::Args,
    /// Loaded here (no [`preload`]): unloaded with the runtime.
    _loader: Option<library_loader::LibraryLoader>,
}

/// The framework is mapped for the life of the process ([`preload`]).
static PRELOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Load the CEF framework now, for [`Runtime::init`] to initialize later; call
/// first thing in `main`, before any other thread exists. Its static
/// initializers install Chromium's allocator as the process's `malloc`, and a
/// thread freeing memory meanwhile lands in the half-installed allocator,
/// which traps (an app that loaded it lazily crashed when a page opened while
/// a tokio worker dropped a map). Whether it loaded: `false` outside a bundle
/// with the framework (a cargo run), where `init` fails as before.
pub fn preload() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Ok(layout) = Layout::for_main(&exe) else { return false };
    if !layout.framework().exists() {
        return false;
    }
    // Never unloaded: CEF can't be loaded twice in a process anyway.
    let loaded = std::mem::ManuallyDrop::new(library_loader::LibraryLoader::new(&exe, false)).load();
    PRELOADED.store(loaded, std::sync::atomic::Ordering::Release);
    loaded
}

impl Runtime {
    /// Loads the framework, makes NSApp CEF-compatible and initializes CEF.
    /// Call on the main thread after NSApp exists (gpui: inside `Application::run`).
    pub fn init(config: Config) -> Result<Runtime> {
        let exe = std::env::current_exe().context("current_exe")?;
        let layout = Layout::for_main(&exe)?;
        let loader = if PRELOADED.load(std::sync::atomic::Ordering::Acquire) {
            None
        } else {
            let loader = library_loader::LibraryLoader::new(&exe, false);
            if !loader.load() {
                bail!("failed to load {}", layout.framework().display());
            }
            Some(loader)
        };
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        crate::cr_app::install(ns_app_class()?)?;

        std::fs::create_dir_all(&config.root).with_context(|| format!("create {}", config.root.display()))?;
        // CEF compares cache paths literally, so resolve symlinks like /var → /private/var.
        let root = config.root.canonicalize()?;
        // Chromium profiles must be direct children of `root_cache_path`.
        let profiles = root.join("profiles");
        crate::profile::private_dir(&profiles)?;
        crate::profile::private_dir(&profiles.join(crate::profile::DEFAULT))?;
        let path = |p: &std::path::Path| CefString::from(p.to_string_lossy().as_ref());
        let settings = Settings {
            no_sandbox: 1,
            browser_subprocess_path: path(&layout.helper),
            framework_dir_path: path(&layout.framework()),
            main_bundle_path: path(&layout.app),
            external_message_pump: 1,
            windowless_rendering_enabled: 1,
            root_cache_path: path(&profiles),
            cache_path: path(&profiles.join(crate::profile::DEFAULT)),
            persist_session_cookies: 1,
            log_file: path(&root.join("cef.log")),
            ..Default::default()
        };
        let mut app = BrowserApp::new(PumpHandler::new(config.schedule), config.switches);
        let args = args::Args::new();
        // Chromium installs its own SIGCHLD handler over tokio's: tokio's child
        // processes (`git`, `curl`…) would never be seen exiting.
        let mut sigchld = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: reads the current action into a zeroed `sigaction`.
        let saved = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), sigchld.as_mut_ptr()) } == 0;
        let initialized = initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) == 1;
        if saved {
            // SAFETY: restores the action read above.
            unsafe { libc::sigaction(libc::SIGCHLD, sigchld.as_ptr(), std::ptr::null_mut()) };
        }
        if !initialized {
            bail!("cef::initialize failed (see {}/cef.log)", root.display());
        }
        Ok(Runtime { profiles_dir: profiles, profiles: Default::default(), _args: args, _loader: loader })
    }
}

impl Runtime {
    /// One `cef::do_message_loop_work`. Call as soon as `Config::schedule` asks (a delay of 0
    /// means now: a frame or an input reply is waiting), and on a ~30 Hz safety tick while
    /// `live_browsers() > 0`: CEF never re-announces delayed work it saw during a pump.
    pub fn pump(&self) {
        do_message_loop_work();
    }

    pub fn live_browsers() -> usize {
        LIVE_BROWSERS.load(Ordering::SeqCst)
    }

    /// Pages paint only when asked for a frame ([`crate::Browser::begin_frame`]): a page
    /// nobody asked for one in its frame period (no window shows it, or its window
    /// stopped drawing) gets one here. Call on every pump tick.
    pub fn drive_pages(&self) {
        let now = Instant::now();
        let pages: Vec<_> = PAGES.with_borrow(|pages| pages.iter().filter_map(std::sync::Weak::upgrade).collect());
        for page in pages {
            let fps = page.frame_rate.load(Ordering::Relaxed);
            if !frame_due(*crate::shared::lock(&page.frame_asked), fps, now) {
                continue;
            }
            let host = crate::shared::lock(&page.browser).as_ref().and_then(|browser| browser.host());
            if let Some(host) = host {
                *crate::shared::lock(&page.frame_asked) = Some(now);
                host.send_external_begin_frame();
            }
        }
    }

    /// The named profile, created on first use. `tunnel` only applies at creation; use
    /// `Profile::set_tunnel` to change it later.
    pub fn profile(&self, name: &str, tunnel: Option<crate::Tunnel>) -> Result<crate::Profile> {
        if let Some(profile) = self.profiles.borrow().get(name) {
            return Ok(profile.clone());
        }
        let profile = match (name, tunnel) {
            (crate::profile::DEFAULT, None) => crate::Profile::global()?,
            (crate::profile::DEFAULT, Some(_)) => {
                bail!("the default profile is local-only; tunnels need a server profile")
            }
            (_, tunnel) => crate::Profile::create(&self.profiles_dir, name, tunnel)?,
        };
        self.profiles.borrow_mut().insert(name.to_owned(), profile.clone());
        Ok(profile)
    }

    /// Pumps until every browser has closed (call `Browser::close` first), then shuts CEF down.
    pub fn shutdown(self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Self::live_browsers() > 0 {
            if Instant::now() > deadline {
                bail!("{} browsers still open; CEF not shut down", Self::live_browsers());
            }
            do_message_loop_work();
            std::thread::sleep(Duration::from_millis(5));
        }
        self.profiles.borrow_mut().clear();
        cef::shutdown();
        Ok(())
    }
}

fn ns_app_class() -> Result<&'static objc2::runtime::AnyClass> {
    let mtm = objc2::MainThreadMarker::new().context("silo-web must init on the main thread")?;
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    // `-class`, not the isa: KVO may have swizzled NSApp to an NSKVONotifying_ subclass.
    let class: &'static objc2::runtime::AnyClass = unsafe { objc2::msg_send![&*app, class] };
    Ok(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undriven_pages_get_a_frame_per_period() {
        let now = Instant::now();
        assert!(frame_due(None, 5, now), "never asked: due");
        assert!(!frame_due(Some(now - Duration::from_millis(100)), 5, now), "5 fps: 200 ms apart");
        assert!(frame_due(Some(now - Duration::from_millis(200)), 5, now));
        assert!(frame_due(Some(now - Duration::from_millis(9)), 120, now));
        assert!(!frame_due(Some(now), 120, now), "a window just asked");
        assert!(frame_due(Some(now), 0, now + Duration::from_secs(1)), "no rate: once a second");
    }
}
