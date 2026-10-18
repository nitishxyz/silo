//! `silo-menubar`: a menu-bar agent (no Dock icon) for the always-on silo
//! server. Shows its status and sessions, opens the app at a session, and
//! starts, stops or restarts the server. One instance per state directory.

mod net;

use std::{
    cell::{Cell, RefCell},
    os::fd::AsFd,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, NSTimer};
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Cmd, Shared};

/// Opens the Silo app: the enclosing `.app` bundle when bundled, else the
/// `silo-app` binary next to this one (development).
#[derive(Clone, Debug)]
pub enum AppLauncher {
    Bundle(PathBuf),
    Binary(PathBuf),
    None,
}

impl AppLauncher {
    fn find() -> Self {
        let Ok(exe) = std::env::current_exe() else { return Self::None };
        let exe = exe.canonicalize().unwrap_or(exe);
        // .../Silo.app/Contents/MacOS/silo-menubar
        if let Some(bundle) = exe.ancestors().nth(3).filter(|p| p.extension().is_some_and(|e| e == "app")) {
            return Self::Bundle(bundle.to_owned());
        }
        match exe.parent().map(|d| d.join("silo-app")).filter(|p| p.is_file()) {
            Some(app) => Self::Binary(app),
            None => Self::None,
        }
    }

    pub fn open(&self, session: Option<&str>) {
        let mut command = match self {
            Self::Bundle(bundle) => {
                let mut c = std::process::Command::new("/usr/bin/open");
                c.arg("-a").arg(bundle);
                if let Some(session) = session {
                    c.arg("--env").arg(format!("SILO_SESSION={session}"));
                }
                c
            }
            Self::Binary(app) => {
                // Development: activate a running instance, else launch one.
                if activate_running("silo-app") {
                    return;
                }
                let mut c = std::process::Command::new(app);
                if let Some(session) = session {
                    c.env("SILO_SESSION", session);
                }
                c.env("SILO_NO_MENUBAR", "1");
                c
            }
            Self::None => return,
        };
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if let Err(error) = command.spawn() {
            tracing::warn!("open app: {error}");
        }
    }
}

fn activate_running(name: &str) -> bool {
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is (do shell script \"pgrep -n -x {name}\") as integer) to true"
    );
    std::process::Command::new("/usr/bin/pgrep").args(["-x", name]).output().is_ok_and(|o| o.status.success())
        && std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).status().is_ok_and(|s| s.success())
}

fn server_exe() -> PathBuf {
    if let Some(path) = std::env::var_os("SILO_SERVER_EXE") {
        return path.into();
    }
    let exe = std::env::current_exe().ok().map(|e| e.canonicalize().unwrap_or(e));
    for name in ["silo-server", "silo"] {
        if let Some(candidate) = exe.as_ref().and_then(|e| e.parent()).map(|d| d.join(name))
            && candidate.is_file()
        {
            return candidate;
        }
    }
    PathBuf::from("silo")
}

#[derive(Clone, Debug)]
enum Action {
    Open,
    OpenSession(String),
    NewSession,
    Start,
    Stop,
    Restart,
    Quit,
}

struct Ivars {
    shared: Arc<Shared>,
    cmds: UnboundedSender<Cmd>,
    app: AppLauncher,
    item: RefCell<Option<Retained<NSStatusItem>>>,
    actions: RefCell<Vec<Action>>,
    seen: Cell<u64>,
    /// Uptime label minute last rendered.
    minute: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SiloMenuBarController"]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            let generation = self.ivars().shared.generation.load(Ordering::Acquire);
            let minute = now_ms() / 60_000;
            if generation != self.ivars().seen.get() || minute != self.ivars().minute.get() {
                self.ivars().seen.set(generation);
                self.ivars().minute.set(minute);
                self.rebuild();
            }
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, item: &NSMenuItem) {
            let index = usize::try_from(item.tag()).unwrap_or(usize::MAX);
            let action = self.ivars().actions.borrow().get(index).cloned();
            let Some(action) = action else { return };
            let send = |cmd| {
                let _ = self.ivars().cmds.send(cmd);
            };
            match action {
                Action::Open => self.ivars().app.open(None),
                Action::OpenSession(id) => send(Cmd::OpenSession(id)),
                Action::NewSession => send(Cmd::NewSession),
                Action::Start => send(Cmd::Start),
                Action::Stop => send(Cmd::Stop),
                Action::Restart => send(Cmd::Restart),
                Action::Quit => {
                    let mtm = MainThreadMarker::from(self);
                    NSApplication::sharedApplication(mtm).terminate(None);
                }
            }
        }
    }
);

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn uptime(started_at: u64) -> String {
    let secs = now_ms().saturating_sub(started_at) / 1000;
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    fn add(&self, menu: &NSMenu, title: &str, action: Option<Action>, key: &str) {
        let mtm = MainThreadMarker::from(self);
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                action.as_ref().map(|_| sel!(menuAction:)),
                &NSString::from_str(key),
            )
        };
        match action {
            Some(action) => {
                let mut actions = self.ivars().actions.borrow_mut();
                item.setTag(actions.len() as isize);
                actions.push(action);
                unsafe { item.setTarget(Some(self)) };
            }
            None => item.setEnabled(false),
        }
        menu.addItem(&item);
    }

    fn rebuild(&self) {
        let mtm = MainThreadMarker::from(self);
        let state = self.ivars().shared.state.lock().map(|s| s.clone()).unwrap_or_default();
        self.ivars().actions.borrow_mut().clear();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let separator = || NSMenuItem::separatorItem(mtm);
        match &state.server {
            Some(info) => {
                self.add(&menu, "Silo Server — Running", None, "");
                self.add(&menu, &format!("v{} · pid {} · up {}", info.version, info.pid, uptime(info.started_at)), None, "");
            }
            None => self.add(&menu, "Silo Server — Stopped", None, ""),
        }
        if let Some(busy) = state.busy {
            self.add(&menu, busy, None, "");
        }
        if let Some(error) = &state.error {
            let short: String = error.chars().take(80).collect();
            self.add(&menu, &format!("⚠︎ {short}"), None, "");
        }
        if state.server.is_some() {
            menu.addItem(&separator());
            if state.sessions.is_empty() {
                self.add(&menu, "No Sessions", None, "");
            } else {
                self.add(&menu, "Sessions", None, "");
                for (i, s) in state.sessions.iter().enumerate() {
                    let windows = if s.windows == 1 { "1 window".to_owned() } else { format!("{} windows", s.windows) };
                    let key = if i < 9 { (i + 1).to_string() } else { String::new() };
                    self.add(&menu, &format!("{}  ·  {windows}", s.name), Some(Action::OpenSession(s.id.clone())), &key);
                }
            }
        }
        menu.addItem(&separator());
        self.add(&menu, "Open Silo", Some(Action::Open), "o");
        self.add(&menu, "New Session", Some(Action::NewSession), "n");
        menu.addItem(&separator());
        if state.server.is_some() {
            self.add(&menu, "Restart Server", Some(Action::Restart), "");
            self.add(&menu, "Stop Server", Some(Action::Stop), "");
        } else {
            self.add(&menu, "Start Server", Some(Action::Start), "");
        }
        menu.addItem(&separator());
        self.add(&menu, "Quit Menu Bar", Some(Action::Quit), "q");
        if let Some(item) = self.ivars().item.borrow().as_ref() {
            item.setMenu(Some(&menu));
            if let Some(button) = item.button(mtm) {
                let symbol = if state.server.is_some() { "terminal.fill" } else { "terminal" };
                if let Some(image) = symbol_image(symbol) {
                    button.setImage(Some(&image));
                }
                let tip = match &state.server {
                    Some(_) => format!("Silo — {} sessions", state.sessions.len()),
                    None => "Silo — server stopped".to_owned(),
                };
                button.setToolTip(Some(&NSString::from_str(&tip)));
            }
        }
    }
}

fn symbol_image(name: &str) -> Option<Retained<NSImage>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), Some(&NSString::from_str("Silo")))?;
    image.setTemplate(true);
    Some(image)
}

/// Held for the agent's lifetime; a second instance exits at once.
fn single_instance(home: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(home).ok()?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("menubar.lock")).ok()?;
    rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).ok()?;
    Some(file)
}

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
    let home = silo_proto::paths::home();
    let Some(_lock) = single_instance(&home) else {
        tracing::info!("another silo-menubar is running for {}", home.display());
        return;
    };
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let shared = Arc::new(Shared::default());
    let launcher = AppLauncher::find();
    let cmds = net::spawn(net::Config { home, server_exe: server_exe(), app: launcher.clone() }, shared.clone());
    let controller = Controller::new(
        mtm,
        Ivars {
            shared,
            cmds,
            app: launcher,
            item: RefCell::new(None),
            actions: RefCell::new(Vec::new()),
            seen: Cell::new(u64::MAX),
            minute: Cell::new(0),
        },
    );
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    *controller.ivars().item.borrow_mut() = Some(item);
    controller.rebuild();
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.5, &controller, sel!(tick:), None, true)
    };
    app.run();
}
