//! `silo-menubar`: a menu-bar agent (no Dock icon) for the always-on silo
//! server. Shows its status, opens the app or its Settings (where the server
//! controls live), and quits Silo. One instance per state directory.

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
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
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

    /// Open the app; with `settings`, at its Settings panel.
    pub fn open(&self, settings: bool) {
        let mut command = match self {
            Self::Bundle(bundle) => {
                // `silo://settings` reaches a running app as well as a
                // launching one (CFBundleURLTypes in the bundle).
                let mut c = std::process::Command::new("/usr/bin/open");
                c.arg("-a").arg(bundle);
                if settings {
                    c.arg(SETTINGS_URL);
                }
                c
            }
            Self::Binary(app) => {
                // Development: activate a running instance (no URL scheme
                // without a bundle), else launch one.
                if activate_running("silo-app") {
                    return;
                }
                let mut c = std::process::Command::new(app);
                if settings {
                    c.env("SILO_OPEN", "settings");
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

const SETTINGS_URL: &str = "silo://settings";

fn activate_running(name: &str) -> bool {
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is (do shell script \"pgrep -n -x {name}\") as integer) to true"
    );
    std::process::Command::new("/usr/bin/pgrep").args(["-x", name]).output().is_ok_and(|o| o.status.success())
        && std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).status().is_ok_and(|s| s.success())
}

#[derive(Clone, Debug)]
enum Action {
    Open,
    Settings,
    Quit,
}

struct Ivars {
    shared: Arc<Shared>,
    cmds: UnboundedSender<Cmd>,
    app: AppLauncher,
    item: RefCell<Option<Retained<NSStatusItem>>>,
    actions: RefCell<Vec<Action>>,
    seen: Cell<u64>,
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
            if self.ivars().shared.quit.load(Ordering::Acquire) {
                NSApplication::sharedApplication(MainThreadMarker::from(self)).terminate(None);
                return;
            }
            let generation = self.ivars().shared.generation.load(Ordering::Acquire);
            if generation != self.ivars().seen.get() {
                self.ivars().seen.set(generation);
                self.rebuild();
            }
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, item: &NSMenuItem) {
            let index = usize::try_from(item.tag()).unwrap_or(usize::MAX);
            let action = self.ivars().actions.borrow().get(index).cloned();
            let Some(action) = action else { return };
            match action {
                Action::Open => self.ivars().app.open(false),
                Action::Settings => self.ivars().app.open(true),
                Action::Quit => {
                    if self.confirm_quit() {
                        let _ = self.ivars().cmds.send(Cmd::Quit);
                    }
                }
            }
        }
    }
);

/// Confirmation text for quitting with `sessions` running, if any are.
fn quit_warning(sessions: usize) -> Option<String> {
    match sessions {
        0 => None,
        1 => Some("This ends 1 running session.".to_owned()),
        n => Some(format!("This ends {n} running sessions.")),
    }
}

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    /// Ask before "Quit Silo" ends running sessions; true to go ahead.
    fn confirm_quit(&self) -> bool {
        let sessions = self.ivars().shared.state.lock().map_or(0, |s| s.running_sessions());
        let Some(warning) = quit_warning(sessions) else { return true };
        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str("Quit Silo?"));
        alert.setInformativeText(&NSString::from_str(&warning));
        alert.addButtonWithTitle(&NSString::from_str("Quit"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        // An accessory app must come forward for its alert to get focus.
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        alert.runModal() == NSAlertFirstButtonReturn
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
        let status = state.status();
        self.add(&menu, &status, None, "");
        if let Some(error) = &state.error {
            let short: String = error.chars().take(80).collect();
            self.add(&menu, &format!("⚠︎ {short}"), None, "");
        }
        self.add(&menu, "Open Silo", Some(Action::Open), "o");
        self.add(&menu, "Settings…", Some(Action::Settings), ",");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(&menu, "Quit Silo", Some(Action::Quit), "q");
        if let Some(item) = self.ivars().item.borrow().as_ref() {
            item.setMenu(Some(&menu));
            if let Some(button) = item.button(mtm) {
                let symbol = if state.server.is_some() { "terminal.fill" } else { "terminal" };
                if let Some(image) = symbol_image(symbol) {
                    button.setImage(Some(&image));
                }
                button.setToolTip(Some(&NSString::from_str(&status)));
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
    let cmds = net::spawn(net::Config { home }, shared.clone());
    let controller = Controller::new(
        mtm,
        Ivars {
            shared,
            cmds,
            app: launcher,
            item: RefCell::new(None),
            actions: RefCell::new(Vec::new()),
            seen: Cell::new(u64::MAX),
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

#[cfg(test)]
mod tests {
    use super::quit_warning;

    #[test]
    fn quit_confirmation_only_with_sessions() {
        assert_eq!(quit_warning(0), None);
        assert_eq!(quit_warning(1).as_deref(), Some("This ends 1 running session."));
        assert_eq!(quit_warning(3).as_deref(), Some("This ends 3 running sessions."));
    }
}
