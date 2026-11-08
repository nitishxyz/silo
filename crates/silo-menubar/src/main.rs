//! `silo-menubar`: a menu-bar agent (no Dock icon) for the always-on silo
//! server, inside Silo Server.app. Its menu: a header (the server's icon,
//! name and status, › how it runs and its log), the app's and server's
//! versions (copyable), Open Silo, Check for Updates, Settings and
//! Permissions (where Silo.app is installed), and Quit Silo. One instance
//! per state directory.

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
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBitmapImageRep,
    NSButton, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSImageView, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypeString, NSStatusBar, NSStatusItem, NSTextField, NSVariableStatusItemLength, NSView, NSWorkspace,
};
use objc2_foundation::{NSAttributedString, NSData, NSDictionary, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSString, NSTimer};
use silo_client::runtime::MENUBAR;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::{Cmd, Shared};

/// Opens the Silo app: the `.app` bundle whose server bundle holds this
/// agent, the `silo-app` binary next to this one (development), else the
/// installed app by bundle id (run from the runtime directory).
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
        // .../Silo.app/Contents/Helpers/Silo Server.app/Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar
        let is_app = |p: &Path| p.extension().is_some_and(|e| e == "app");
        fn holder(bundle: &Path) -> Option<&Path> {
            bundle
                .parent()
                .filter(|h| h.ends_with("Contents/Helpers"))
                .and_then(|h| h.parent()?.parent())
                .filter(|b| b.extension().is_some_and(|e| e == "app"))
        }
        if let Some(agent) = exe.ancestors().nth(3).filter(|p| is_app(p)) {
            return match holder(agent).and_then(holder) {
                Some(app) => Self::Bundle(app.to_owned()),
                None => Self::Installed,
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
    Updates,
}

impl Page {
    const fn name(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Permissions => "permissions",
            Self::Updates => "updates",
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
    Updates,
    ShowLog,
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
                let notice = self.ivars().shared.notice.lock().ok().and_then(|mut n| n.take());
                if let Some(notice) = notice {
                    self.show_notice(notice);
                }
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
                Action::Updates => self.check_for_updates(),
                Action::ShowLog => {
                    let log = silo_proto::paths::home().join("server.log");
                    let _ = std::process::Command::new("/usr/bin/open").args(["-a", "Console"]).arg(log).spawn();
                }
                Action::Quit => {
                    if self.confirm_quit() {
                        let _ = self.ivars().cmds.send(Cmd::Quit);
                    }
                }
            }
        }

        /// Development (`SILO_MENUBAR_OPEN`): the menu, popped up at the
        /// top right of the main screen.
        #[unsafe(method(debugPopUp:))]
        fn debug_pop_up(&self, _: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let menu = self.ivars().item.borrow().as_ref().and_then(|i| i.menu(mtm));
            let frame = objc2_app_kit::NSScreen::mainScreen(mtm).map(|s| s.frame());
            if let (Some(menu), Some(frame)) = (menu, frame) {
                let at = NSPoint::new(frame.size.width - 420.0, frame.size.height - 34.0);
                menu.popUpMenuPositioningItem_atLocation_inView(None, at, None);
            }
        }

        /// The version block's copy button: versions and platform, for a bug report.
        #[unsafe(method(copyVersions:))]
        fn copy_versions(&self, button: &NSButton) {
            let state = self.ivars().shared.state.lock().map(|s| s.clone()).unwrap_or_default();
            let text = versions_text(&versions(&state, installed_app_version().as_deref()));
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            pasteboard.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
            if let Some(done) = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("checkmark"), Some(&NSString::from_str("Copied"))) {
                button.setImage(Some(&done));
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

    /// The header's submenu: how the server runs, and its log.
    fn server_menu(&self, state: &net::MenuState) -> Retained<NSMenu> {
        let mtm = MainThreadMarker::from(self);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        if let Some(server) = &state.server {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            self.add(&menu, &format!("Started {}", ago(now.saturating_sub(server.started_at) / 1000)), None, "");
            self.add(&menu, &format!("Process {}", server.pid), None, "");
            let remote = match &server.remote_error {
                None => "Remote access: on".to_owned(),
                Some(error) => format!("Remote access: off ({})", error.chars().take(60).collect::<String>()),
            };
            self.add(&menu, &remote, None, "");
        } else {
            self.add(&menu, &state.headline().1, None, "");
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(&menu, "Show Server Log", Some(Action::ShowLog), "");
        menu
    }

    /// Two label/value rows and a copy button, as one menu item's view.
    fn versions_view(&self, rows: &[(String, String)]) -> Retained<NSView> {
        let mtm = MainThreadMarker::from(self);
        let (width, row, pad) = (300.0, 16.0, 7.0);
        let height = row * rows.len() as f64 + pad * 2.0;
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let font = NSFont::systemFontOfSize(11.0);
        for (i, (label, value)) in rows.iter().enumerate() {
            // Bottom-up coordinates: the first row on top.
            let y = height - pad - row * (i as f64 + 1.0);
            for (text, x, w, color) in
                [(label, 14.0, 88.0, NSColor::tertiaryLabelColor()), (value, 102.0, width - 102.0 - 44.0, NSColor::secondaryLabelColor())]
            {
                let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
                field.setFont(Some(&font));
                field.setTextColor(Some(&color));
                field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
                field.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, row - 1.0)));
                view.addSubview(&field);
            }
        }
        let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("doc.on.doc"),
            Some(&NSString::from_str("Copy versions")),
        );
        if let Some(symbol) = symbol {
            let button = unsafe { NSButton::buttonWithImage_target_action(&symbol, Some(self), Some(sel!(copyVersions:)), mtm) };
            button.setBordered(false);
            button.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
            button.setToolTip(Some(&NSString::from_str("Copy versions")));
            button.setFrame(NSRect::new(NSPoint::new(width - 38.0, (height - 22.0) / 2.0), NSSize::new(22.0, 22.0)));
            button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            view.addSubview(&button);
        }
        view
    }

    /// The header: the server's icon, its name over a status dot and line.
    fn header_view(&self, name: &str, health: net::Health, headline: &str) -> Retained<NSView> {
        let mtm = MainThreadMarker::from(self);
        // A 44 pt icon; the name and status lines centred on it as a block.
        let (width, height, icon) = (300.0, 62.0, 44.0);
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        if let Some(image) = server_icon() {
            let image = NSImageView::imageViewWithImage(&image, mtm);
            image.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyUpOrDown);
            image.setFrame(NSRect::new(NSPoint::new(14.0, (height - icon) / 2.0), NSSize::new(icon, icon)));
            view.addSubview(&image);
        }
        let x = 14.0 + icon + 12.0;
        let (name_h, status_h, gap) = (19.0, 16.0, 2.0);
        let bottom = (height - (name_h + gap + status_h)) / 2.0;
        let (name_line, status_line) = header_lines(name, health, headline);
        for (text, y, h) in [(status_line, bottom, status_h), (name_line, bottom + status_h + gap, name_h)] {
            let label = NSTextField::labelWithAttributedString(&text, mtm);
            label.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
            label.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(width - x - 14.0, h)));
            view.addSubview(&label);
        }
        view
    }

    /// Check for Updates: the app's own check where Silo.app is installed;
    /// on a server-only Mac the server bundle's `silo update --check`, then
    /// `silo update` if the user agrees (answers come back through the tick).
    fn check_for_updates(&self) {
        if installed_app_version().is_some() || !matches!(self.ivars().app, AppLauncher::Installed) {
            self.ivars().app.open(Some(Page::Updates));
            return;
        }
        run_update(&self.ivars().shared, false);
    }

    /// Answer a finished update check or update.
    fn show_notice(&self, notice: net::Notice) {
        let mtm = MainThreadMarker::from(self);
        let alert = NSAlert::new(mtm);
        let server = silo_proto::CHANNEL.server_name();
        let text = notice.text.trim();
        let available = notice.ok && !notice.update && text.contains("available");
        let (title, info) = match (notice.update, notice.ok) {
            (false, true) if available => (format!("A new {server} is available"), text.to_owned()),
            (false, true) => (format!("{server} is up to date"), text.to_owned()),
            (true, true) => (format!("{server} was updated"), text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or(text).to_owned()),
            (_, false) => ("Couldn't check for updates".to_owned(), text.to_owned()),
        };
        alert.setMessageText(&NSString::from_str(&title));
        alert.setInformativeText(&NSString::from_str(&info));
        if available {
            alert.addButtonWithTitle(&NSString::from_str("Update"));
            alert.addButtonWithTitle(&NSString::from_str("Later"));
        }
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        if alert.runModal() == NSAlertFirstButtonReturn && available {
            run_update(&self.ivars().shared, true);
        }
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

    fn add(&self, menu: &NSMenu, title: &str, action: Option<Action>, key: &str) -> Retained<NSMenuItem> {
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
        item
    }

    fn rebuild(&self) {
        let mtm = MainThreadMarker::from(self);
        let state = self.ivars().shared.state.lock().map(|s| s.clone()).unwrap_or_default();
        self.ivars().actions.borrow_mut().clear();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let channel = silo_proto::CHANNEL;
        let name = channel.app_name();
        let app_version = installed_app_version();
        let has_app = app_version.is_some() || !matches!(self.ivars().app, AppLauncher::Installed);

        // The header: icon, "Silo Server", a dot and what it is doing; › server details.
        let (health, headline) = state.headline();
        let header = NSMenuItem::new(mtm);
        header.setView(Some(&self.header_view(channel.server_name(), health, &headline)));
        menu.addItem(&header);
        if let Some(error) = &state.error {
            let short: String = error.chars().take(80).collect();
            self.add(&menu, &format!("⚠︎ {short}"), None, "");
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Versions, with a copy button.
        let rows = versions(&state, app_version.as_deref());
        let block = NSMenuItem::new(mtm);
        block.setView(Some(&self.versions_view(&rows)));
        menu.addItem(&block);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        if has_app {
            self.add(&menu, &format!("Open {name}"), Some(Action::Open), "o");
        }
        let server = self.add(&menu, "Server", None, "");
        server.setEnabled(true);
        server.setSubmenu(Some(&self.server_menu(&state)));
        self.add(&menu, "Check for Updates…", Some(Action::Updates), "");
        if has_app {
            self.add(&menu, "Settings…", Some(Action::Settings), ",");
            self.add(&menu, "Permissions…", Some(Action::Permissions), "");
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(&menu, &format!("Quit {name}"), Some(Action::Quit), "q");
        let status = state.status();
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

/// "2 hours ago" for `secs` seconds.
fn ago(secs: u64) -> String {
    let (n, unit) = match secs {
        0..60 => return "just now".to_owned(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// The header's two lines: the name in bold, then a coloured dot and what
/// the server is doing.
fn header_lines(name: &str, health: net::Health, headline: &str) -> (Retained<NSAttributedString>, Retained<NSAttributedString>) {
    let dot = match health {
        net::Health::Running => NSColor::systemGreenColor(),
        net::Health::Busy => NSColor::systemBlueColor(),
        net::Health::Trouble => NSColor::systemOrangeColor(),
        net::Health::Stopped => NSColor::tertiaryLabelColor(),
    };
    let bold = NSFont::systemFontOfSize_weight(15.0, unsafe { objc2_app_kit::NSFontWeightSemibold });
    let small = NSFont::systemFontOfSize(12.0);
    let name_line = styled(&[(name, &bold, NSColor::labelColor())]);
    let status_line = styled(&[("\u{25cf} ", &small, dot), (headline, &small, NSColor::secondaryLabelColor())]);
    (name_line, status_line)
}

/// Runs of text, each in its font and colour.
fn styled(parts: &[(&str, &NSFont, Retained<NSColor>)]) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    for (text, font, color) in parts {
        let keys: [&objc2_foundation::NSAttributedStringKey; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
        let values: [&AnyObject; 2] = [*font, &**color];
        let attrs = NSDictionary::from_slices(&keys, &values);
        let part = unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(text), &attrs) };
        out.appendAttributedString(&part);
    }
    Retained::into_super(out)
}

/// The server bundle's icon (the agent runs inside it), else the
/// app's: its `Silo.icns` (a menu item draws a plain image; the asset
/// catalog's layered icon may not), else what Finder shows.
fn server_icon() -> Option<Retained<NSImage>> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let workspace = NSWorkspace::sharedWorkspace();
    let bundle = match exe.ancestors().nth(6).filter(|b| b.extension().is_some_and(|e| e == "app")) {
        Some(bundle) => bundle.to_owned(),
        None => PathBuf::from(workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(BUNDLE_ID))?.path()?.to_string()),
    };
    let icns = bundle.join("Contents/Resources/Silo.icns");
    let icon = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&icns.to_string_lossy()))
        .unwrap_or_else(|| workspace.iconForFile(&NSString::from_str(&bundle.to_string_lossy())));
    icon.setSize(NSSize::new(44.0, 44.0));
    Some(icon)
}

/// This channel's installed Silo.app version: the app holding this agent,
/// else the one Launch Services knows by bundle id.
fn installed_app_version() -> Option<String> {
    let holder = match AppLauncher::of(&std::env::current_exe().ok()?.canonicalize().ok()?) {
        AppLauncher::Bundle(app) => Some(app),
        _ => None,
    };
    let (path, held) = match holder {
        Some(app) => (app, true),
        None => (
            PathBuf::from(NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&NSString::from_str(BUNDLE_ID))?.path()?.to_string()),
            false,
        ),
    };
    let plist = std::fs::read_to_string(path.join("Contents/Info.plist")).ok()?;
    let version = silo_client::update::plist_string(&plist, "CFBundleShortVersionString").map(str::to_owned);
    // A dev bundle declares none: the app holding this agent is this build.
    version.or_else(|| held.then(|| env!("CARGO_PKG_VERSION").to_owned()))
}

/// The version rows: the app (when installed) and the server.
fn versions(state: &net::MenuState, app: Option<&str>) -> Vec<(String, String)> {
    let channel = silo_proto::CHANNEL;
    let mut rows = Vec::new();
    if let Some(app) = app {
        let kind = if channel == silo_proto::Channel::Dev { "Dev" } else { "Release" };
        rows.push((channel.app_name().to_owned(), format!("{app} \u{b7} {kind}")));
    }
    let server = match (&state.server, &state.build) {
        (Some(info), Some(build)) => format!("{} ({})", info.version, build.chars().take(8).collect::<String>()),
        (Some(info), None) => info.version.clone(),
        (None, _) => format!("{} ({})", env!("CARGO_PKG_VERSION"), silo_proto::BUILD.chars().take(8).collect::<String>()),
    };
    rows.push((channel.server_name().to_owned(), server));
    rows
}

/// What the copy button copies: every row, and the platform.
fn versions_text(rows: &[(String, String)]) -> String {
    let mut lines: Vec<String> = rows.iter().map(|(k, v)| format!("{k} {v}")).collect();
    let os = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    lines.push(format!("macOS {} {}", os.unwrap_or_default(), std::env::consts::ARCH));
    lines.join("\n")
}

/// Run the runtime server's `silo update --check` (`update`: `silo
/// update`) off the main thread; its answer becomes [`Shared::notice`].
fn run_update(shared: &Arc<Shared>, update: bool) {
    let shared = shared.clone();
    let silo = silo_client::runtime::server_exe(&silo_proto::paths::home());
    std::thread::spawn(move || {
        let args: &[&str] = if update { &["update"] } else { &["update", "--check"] };
        let (ok, text) = match std::process::Command::new(&silo).args(args).stdin(std::process::Stdio::null()).output() {
            Ok(out) => {
                let text = if out.status.success() { out.stdout } else { [out.stdout, out.stderr].concat() };
                (out.status.success(), String::from_utf8_lossy(&text).trim().to_owned())
            }
            Err(error) => (false, format!("run {}: {error}", silo.display())),
        };
        shared.post(net::Notice { update, ok, text });
    });
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
    // Development: open the menu a moment after launch (screenshots, checks).
    #[cfg(debug_assertions)]
    if std::env::var_os("SILO_MENUBAR_OPEN").is_some() {
        let _: () =
            unsafe { msg_send![&*controller, performSelector: sel!(debugPopUp:), withObject: std::ptr::null::<AnyObject>(), afterDelay: 1.5f64] };
    }
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.5, &controller, sel!(tick:), None, true) };
    app.run();
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{AppLauncher, ago, quit_warning, stale_holder, versions, versions_text};

    #[test]
    fn version_rows_and_ages() {
        let mut state = crate::net::MenuState::default();
        let rows = versions(&state, Some("0.1.27"));
        assert_eq!(rows.len(), 2, "the app when installed, and the server");
        assert_eq!(rows[0].0, silo_proto::CHANNEL.app_name());
        assert!(rows[0].1.starts_with("0.1.27 · "), "{:?}", rows[0]);
        assert!(rows[1].1.starts_with(env!("CARGO_PKG_VERSION")), "no server: this build's");
        state.server = Some(silo_proto::control::ServerInfo {
            version: "9.9.9".into(),
            pid: 1,
            server_id: String::new(),
            started_at: 0,
            endpoint_id: String::new(),
            hostname: String::new(),
            remote_error: None,
            endpoint_addr: None,
            idle_exit_secs: 0,
        });
        state.build = Some("2f021a5309403e44".into());
        let rows = versions(&state, None);
        assert_eq!(rows, [(silo_proto::CHANNEL.server_name().to_owned(), "9.9.9 (2f021a53)".to_owned())], "server-only: one row");
        let text = versions_text(&rows);
        assert!(text.starts_with(&format!("{} 9.9.9 (2f021a53)\nmacOS ", silo_proto::CHANNEL.server_name())), "{text}");
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(7300), "2 hours ago");
        assert_eq!(ago(3 * 86_400), "3 days ago");
    }

    #[test]
    fn opens_the_app_holding_the_helper() {
        let helper = |app: &str, server: &str, name: &str| {
            PathBuf::from(format!("{app}/Contents/Helpers/{server}.app/Contents/Helpers/{name}.app/Contents/MacOS/silo-menubar"))
        };
        let release = helper("/Applications/Silo.app", "Silo Server", "Silo Menu Bar");
        assert_eq!(AppLauncher::of(&release), AppLauncher::Bundle("/Applications/Silo.app".into()));
        let dev = helper("/w/target/debug/SiloDev.app", "SiloDev Server", "SiloDev Menu Bar");
        assert_eq!(AppLauncher::of(&dev), AppLauncher::Bundle("/w/target/debug/SiloDev.app".into()));
        // The runtime copy (whose server bundle is in no app), and agents in no server bundle.
        let runtime = "/h/Library/Application Support/silo/runtime/Silo Server.app/Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar";
        assert_eq!(AppLauncher::of(Path::new(runtime)), AppLauncher::Installed);
        assert_eq!(AppLauncher::of(Path::new("/x/Silo Menu Bar.app/Contents/MacOS/silo-menubar")), AppLauncher::Installed);
        assert_eq!(AppLauncher::of(Path::new("/x/Silo.app/Contents/Helpers/Silo Menu Bar.app/Contents/MacOS/silo-menubar")), AppLauncher::Installed);
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
