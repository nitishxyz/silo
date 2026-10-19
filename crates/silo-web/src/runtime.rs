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

impl Config {
    /// `~/Library/Application Support/Silo/web`.
    pub fn default_root() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME unset")?;
        Ok(PathBuf::from(home).join("Library/Application Support/Silo/web"))
    }
}

/// The CEF engine. One per process, main thread only.
pub struct Runtime {
    profiles_dir: PathBuf,
    profiles: std::cell::RefCell<std::collections::HashMap<String, crate::Profile>>,
    _args: args::Args,
    _loader: library_loader::LibraryLoader,
}

impl Runtime {
    /// Loads the framework, makes NSApp CEF-compatible and initializes CEF.
    /// Call on the main thread after NSApp exists (gpui: inside `Application::run`).
    pub fn init(config: Config) -> Result<Runtime> {
        let exe = std::env::current_exe().context("current_exe")?;
        let layout = Layout::for_main(&exe)?;
        let loader = library_loader::LibraryLoader::new(&exe, false);
        if !loader.load() {
            bail!("failed to load {}", layout.framework().display());
        }
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
        if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
            bail!("cef::initialize failed (see {}/cef.log)", root.display());
        }
        Ok(Runtime { profiles_dir: profiles, profiles: Default::default(), _args: args, _loader: loader })
    }
}

impl Runtime {
    /// One `cef::do_message_loop_work`. Call when `Config::schedule` fires, and on a ~30 Hz
    /// safety tick while `live_browsers() > 0`.
    pub fn pump(&self) {
        do_message_loop_work();
    }

    pub fn live_browsers() -> usize {
        LIVE_BROWSERS.load(Ordering::SeqCst)
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
