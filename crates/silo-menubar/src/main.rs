//! `silo-menubar`: a menu-bar agent (no Dock icon) for the always-on silo
//! server. Shows its status, opens the app, its Settings (where the server
//! controls live) or its Permissions panel, and quits Silo. One instance per state directory.

mod net;

use std::{
    cell::{Cell, RefCell},
    os::fd::AsFd,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSBitmapImageRep, NSImage, NSMenu, NSMenuItem,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString, NSTimer};
use silo_client::runtime::MENUBAR;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Cmd, Shared};

/// Opens the Silo app: the `.app` bundle whose `Contents/Helpers` holds this
/// agent's helper bundle, the `silo-app` binary next to this one
/// (development), else the installed app by bundle id (run from the runtime
/// directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppLauncher {
    Bundle(PathBuf),
    Binary(PathBuf),
    Installed,
}

impl AppLauncher {
    fn find() -> Self {
        let Ok(exe) = std::env::current_exe() else {
            return Self::Installed;
        };
        let exe = exe.canonicalize().unwrap_or(exe);
        Self::of(&exe)
    }

    fn of(exe: &Path) -> Self {
        // .../Silo.app/Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar
        let helper = exe.ancestors().nth(3).filter(|p| p.extension().is_some_and(|e| e == "app"));
        if let Some(helper) = helper {
            return match helper.parent().filter(|h| h.ends_with("Contents/Helpers")).and_then(|h| h.parent()?.parent()) {
                Some(app) if app.extension().is_some_and(|e| e == "app") => Self::Bundle(app.to_owned()),
                _ => Self::Installed,
            };
        }
        match exe.parent().map(|d| d.join("silo-app")).filter(|p| p.is_file()) {
            Some(app) => Self::Binary(app),
            None => Self::Installed,
        }
    }

    /// Open the app; with `page`, at its Settings or Permissions panel.
    pub fn open(&self, page: Option<Page>) {
        let mut command = match self {
            Self::Bundle(_) | Self::Installed => {
                // `silo://settings` (`silo://permissions`) reaches a running
                // app as well as a launching one (CFBundleURLTypes in the
                // bundle).
                let mut c = std::process::Command::new("/usr/bin/open");
                match self {
                    Self::Bundle(bundle) => c.arg("-a").arg(bundle),
                    _ => c.args(["-b", BUNDLE_ID]),
                };
                if let Some(page) = page {
                    c.arg(format!("silo://{}", page.name()));
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
                if let Some(page) = page {
                    c.env("SILO_OPEN", page.name());
                }
                c.env("SILO_NO_MENUBAR", "1");
                c
            }
        };
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if let Err(error) = command.spawn() {
            tracing::warn!("open app: {error}");
        }
    }
}

/// A panel of the app the agent opens (`silo://<name>`, `SILO_OPEN`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Settings,
    Permissions,
}

impl Page {
    const fn name(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Permissions => "permissions",
        }
    }
}
/// This channel's app (`xyz.nitish.silo`, dev `xyz.nitish.silo.dev`).
const BUNDLE_ID: &str = silo_proto::CHANNEL.bundle_id();

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
    Permissions,
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
                tracing::info!("quitting");
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
                Action::Open => self.ivars().app.open(None),
                Action::Settings => self.ivars().app.open(Some(Page::Settings)),
                Action::Permissions => self.ivars().app.open(Some(Page::Permissions)),
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
        let Some(warning) = quit_warning(sessions) else {
            return true;
        };
        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(&format!("Quit {}?", silo_proto::CHANNEL.app_name())));
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
        let name = silo_proto::CHANNEL.app_name();
        self.add(&menu, &format!("Open {name}"), Some(Action::Open), "o");
        self.add(&menu, "Settings…", Some(Action::Settings), ",");
        self.add(&menu, "Permissions…", Some(Action::Permissions), "");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(&menu, &format!("Quit {name}"), Some(Action::Quit), "q");
        if let Some(item) = self.ivars().item.borrow().as_ref() {
            item.setMenu(Some(&menu));
            if let Some(button) = item.button(mtm) {
                if let Some(image) = mark_image() {
                    button.setImage(Some(&image));
                }
                // The mark dims while no server runs.
                button.setAppearsDisabled(state.server.is_none());
                // Dev builds say so beside the mark, so the two agents never look alike.
                if silo_proto::CHANNEL == silo_proto::Channel::Dev {
                    button.setTitle(&NSString::from_str("Dev"));
                    button.setImagePosition(objc2_app_kit::NSCellImagePosition::ImageLeading);
                }
                button.setToolTip(Some(&NSString::from_str(&status)));
            }
        }
    }
}

/// The Silo mark as an 18 pt template image (`scripts/make-icons.sh`),
/// built in (an unbundled dev agent has no Resources).
fn mark_image() -> Option<Retained<NSImage>> {
    const PNGS: [&[u8]; 2] = [include_bytes!("../assets/menubar.png"), include_bytes!("../assets/menubar@2x.png")];
    let size = NSSize { width: 18.0, height: 18.0 };
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    for png in PNGS {
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png))?;
        rep.setSize(size);
        image.addRepresentation(&rep);
    }
    image.setTemplate(true);
    image.setAccessibilityDescription(Some(&NSString::from_str(silo_proto::CHANNEL.app_name())));
    Some(image)
}

/// Held for the agent's lifetime, recording `<pid> <build> <exe>`. A
/// second agent of this build exits at once; one of another build asks the
/// recorded agent to quit (if that pid still runs that `silo-menubar`) and
/// takes over, so no old agent keeps an old Silo.app in use.
fn single_instance(home: &Path) -> Option<std::fs::File> {
    use std::io::{Read, Seek, Write};
    std::fs::create_dir_all(home).ok()?;
    let mut file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(home.join("menubar.lock")).ok()?;
    let lock = |file: &std::fs::File| rustix::fs::flock(file.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok();
    if !lock(&file) {
        let mut record = String::new();
        file.read_to_string(&mut record).ok()?;
        let pid = stale_holder(&record, silo_proto::BUILD, silo_client::runtime::name_of)?;
        tracing::info!(pid, "replacing the menu-bar agent of another build");
        let _ = rustix::process::kill_process(rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)?, rustix::process::Signal::TERM);
        (0..50).find(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            lock(&file)
        })?;
    }
    let me = silo_client::runtime::exe_of(std::process::id()).unwrap_or_default();
    file.set_len(0).ok()?;
    file.rewind().ok()?;
    write!(file, "{} {} {}", std::process::id(), silo_proto::BUILD, me.display()).ok()?;
    Some(file)
}

/// The pid of the agent `record` names, if it is of another build than
/// `build` and still runs a `silo-menubar` (from any layout: a helper
/// bundle, the runtime copy, an older app's `Contents/MacOS`). By process
/// name, not path: a runtime bundle swapped out under a running agent moves
/// and goes away. The record's pid holds the lock (so it has not exited),
/// which leaves no reused pid to mistake.
fn stale_holder(record: &str, build: &str, name_of: impl Fn(u32) -> Option<String>) -> Option<u32> {
    let mut parts = record.splitn(3, ' ');
    let (pid, holder_build, exe) = (parts.next()?.parse().ok()?, parts.next()?, Path::new(parts.next()?));
    let ours = holder_build == build || pid == std::process::id();
    (!ours && exe.file_name()? == MENUBAR && name_of(pid).as_deref() == Some(MENUBAR)).then_some(pid)
}

/// Past this the log starts over, the previous one kept as `menubar.log.old`.
const LOG_CAP: u64 = 1024 * 1024;

/// `<home>/menubar.log`, appended: whoever starts the agent (the app, the
/// `silo` CLI) discards its output.
fn log_file(home: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(home).ok()?;
    let path = home.join("menubar.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = std::fs::rename(&path, home.join("menubar.log.old"));
    }
    std::fs::File::options().create(true).append(true).open(path).ok()
}

fn main() {
    // Started by something that left its app identity (Silo.app's), the
    // agent would register as that app and take its `silo://` URLs; drop it
    // before AppKit reads it (single-threaded here).
    for var in silo_proto::APP_IDENTITY_ENV {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::remove_var(var) };
    }
    let home = silo_proto::paths::home();
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info".into());
    match log_file(&home) {
        Some(file) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
        }
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
    let Some(_lock) = single_instance(&home) else {
        tracing::info!("another silo-menubar is running for {}", home.display());
        return;
    };
    tracing::info!(pid = std::process::id(), build = silo_proto::BUILD, "menu-bar agent started");
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let shared = Arc::new(Shared::default());
    let launcher = AppLauncher::find();
    let cmds = net::spawn(net::Config { home }, shared.clone());
    let controller = Controller::new(
        mtm,
        Ivars { shared, cmds, app: launcher, item: RefCell::new(None), actions: RefCell::new(Vec::new()), seen: Cell::new(u64::MAX) },
    );
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    *controller.ivars().item.borrow_mut() = Some(item);
    controller.rebuild();
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.5, &controller, sel!(tick:), None, true) };
    app.run();
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{AppLauncher, quit_warning, stale_holder};

    #[test]
    fn opens_the_app_holding_the_helper() {
        let helper = |app: &str, name: &str| PathBuf::from(format!("{app}/Contents/Helpers/{name}.app/Contents/MacOS/silo-menubar"));
        let release = helper("/Applications/Silo.app", "Silo Menu Bar");
        assert_eq!(AppLauncher::of(&release), AppLauncher::Bundle("/Applications/Silo.app".into()));
        let dev = helper("/w/target/debug/SiloDev.app", "SiloDev Menu Bar");
        assert_eq!(AppLauncher::of(&dev), AppLauncher::Bundle("/w/target/debug/SiloDev.app".into()));
        // The runtime copy, and a bundle in no app's Helpers.
        assert_eq!(AppLauncher::of(Path::new("/h/Library/Application Support/silo/runtime/silo-menubar")), AppLauncher::Installed);
        assert_eq!(AppLauncher::of(Path::new("/x/Silo Menu Bar.app/Contents/MacOS/silo-menubar")), AppLauncher::Installed);
        assert_eq!(AppLauncher::of(Path::new("/x/Silo.app/Contents/Other/Silo Menu Bar.app/Contents/MacOS/silo-menubar")), AppLauncher::Installed);
        // Development: `silo-app` beside it.
        let dir = std::env::temp_dir().join(format!("silo-menubar-launcher-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(AppLauncher::of(&dir.join("silo-menubar")), AppLauncher::Installed);
        std::fs::write(dir.join("silo-app"), "").unwrap();
        assert_eq!(AppLauncher::of(&dir.join("silo-menubar")), AppLauncher::Binary(dir.join("silo-app")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replaces_only_a_live_agent_of_another_build() {
        let old = "/h/runtime/Silo Menu Bar.app/Contents/MacOS/silo-menubar";
        let running = |pid: u32| (pid == 10).then(|| "silo-menubar".to_owned());
        assert_eq!(stale_holder(&format!("10 oldbuild {old}"), "new", running), Some(10));
        // Older layouts: the bare runtime copy, an older app's `Contents/MacOS`.
        assert_eq!(stale_holder("10 oldbuild /h/runtime/silo-menubar", "new", running), Some(10));
        assert_eq!(stale_holder("10 oldbuild /Applications/Silo.app/Contents/MacOS/silo-menubar", "new", running), Some(10));
        // Same build (dev, a second launch): leave it.
        assert_eq!(stale_holder(&format!("10 new {old}"), "new", running), None);
        // The pid is gone or now runs something else.
        assert_eq!(stale_holder(&format!("11 oldbuild {old}"), "new", running), None);
        assert_eq!(stale_holder(&format!("10 oldbuild {old}"), "new", |_| Some("vim".to_owned())), None);
        assert_eq!(stale_holder("10 oldbuild /h/runtime/old b/vim", "new", running), None);
        // No or a garbled record.
        assert_eq!(stale_holder("", "new", running), None);
        assert_eq!(stale_holder("x oldbuild a", "new", running), None);
    }

    #[test]
    fn quit_confirmation_only_with_sessions() {
        assert_eq!(quit_warning(0), None);
        assert_eq!(quit_warning(1).as_deref(), Some("This ends 1 running session."));
        assert_eq!(quit_warning(3).as_deref(), Some("This ends 3 running sessions."));
    }
}
