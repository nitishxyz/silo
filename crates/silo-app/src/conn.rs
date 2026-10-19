//! Host connections on the tokio runtime: connect through the host's
//! transport, forward events to the UI, and reconnect when the connection
//! drops.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use silo_client::{
    Control, DifferentBuild,
    mismatch::Upgraded,
    service::{self, Action, Manager, Service},
};
use silo_proto::control::Event;
use tokio::sync::mpsc;

use crate::hosts::Transport;

#[allow(clippy::large_enum_variant, reason = "sent one at a time over a channel")]
pub enum ConnEvent {
    Connected(Control),
    Event(Event),
    Disconnected(String),
    /// A server of a different build holds this host; nothing was spawned.
    /// Sent again only when its description changes.
    DifferentBuild(DifferentBuild),
    /// The remote server reported a new route (already used for reconnects).
    Addr(silo_client::transport::EndpointAddr),
    /// The remote server refused this device; the loop stops.
    NotPaired,
    /// The remote server is another build; polled slowly.
    RemoteBuild,
    /// "Quit Silo" stopped the server on purpose: quit, don't reconnect.
    Quit,
}

/// How often to look again while an incompatible server holds the host.
const INCOMPATIBLE_POLL: Duration = Duration::from_secs(2);
/// Retry delay while a remote server hands over to a new build.
const UPGRADING_RETRY: Duration = Duration::from_millis(300);
/// How often a connected remote host's address is re-read.
const ADDR_REFRESH: Duration = Duration::from_secs(300);

/// Why a connect attempt failed, as far as the UI cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Unreachable,
    Upgrading,
    NotPaired,
    DifferentBuild,
}

#[must_use]
pub fn classify(error: &anyhow::Error) -> Failure {
    if error.downcast_ref::<silo_client::Upgrading>().is_some() {
        Failure::Upgrading
    } else if error.downcast_ref::<silo_client::Unauthorized>().is_some() {
        Failure::NotPaired
    } else if error.downcast_ref::<silo_client::BuildMismatch>().is_some() {
        Failure::DifferentBuild
    } else {
        Failure::Unreachable
    }
}

/// Delay before retry `attempt` (0-based): doubling from 100 ms locally or
/// 250 ms remotely, capped at 3 s / 15 s, scaled by `jitter` in [0, 1] to
/// 50–100 %.
#[must_use]
pub fn backoff(attempt: u32, remote: bool, jitter: f64) -> Duration {
    let (base, cap): (u64, u64) = if remote { (250, 15_000) } else { (100, 3_000) };
    let full = base.saturating_mul(1u64 << attempt.min(16)).min(cap);
    Duration::from_millis(full).mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1000) / 999.0
}

/// The server binary: `silo-server` (app bundle) or `silo` (cargo target
/// dir) next to this executable, else `silo` on `PATH`.
#[must_use]
pub fn server_exe() -> PathBuf {
    server_exe_path()
}

/// How this app introduces itself to servers.
#[must_use]
pub fn client_name() -> String {
    format!("Silo {} (macOS, pid {})", env!("CARGO_PKG_VERSION"), std::process::id())
}

fn server_exe_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SILO_SERVER_EXE") {
        return path.into();
    }
    if let Some(path) = sibling(&["silo-server", "silo"]) {
        return path;
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("silo");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("silo")
}

const BUNDLE_ID: &str = "xyz.nitish.silo";

/// How this Mac's server runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Launch {
    /// As the launchd agent: in the background, at login.
    Launchd,
    /// A plain server for this app's state directory.
    Plain,
    /// Plain, as Silo runs from outside Applications (a disk image, Downloads).
    Unmoved,
}

/// What decides [`Launch`].
struct Origin<'a> {
    silo_home: Option<&'a OsStr>,
    release: bool,
    /// This executable, canonical.
    app_exe: &'a Path,
    server_exe: &'a Path,
    bundle_id: Option<&'a str>,
    user_home: &'a Path,
}

/// launchd only for a release Silo.app in (~)/Applications running its own
/// bundled server for the default state directory.
fn launch(o: &Origin) -> Launch {
    if o.silo_home.is_some_and(|h| !h.is_empty()) || !o.release {
        return Launch::Plain;
    }
    let Some(app) = bundle_of(o.app_exe) else { return Launch::Plain };
    if o.server_exe != app.join("Contents/MacOS/silo-server") || o.bundle_id != Some(BUNDLE_ID) {
        return Launch::Plain;
    }
    match app.parent() {
        Some(dir) if dir == Path::new("/Applications") || dir == o.user_home.join("Applications") => Launch::Launchd,
        _ => Launch::Unmoved,
    }
}

/// `X.app` for `X.app/Contents/MacOS/Silo`.
fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent().filter(|_| exe.file_name() == Some(OsStr::new("Silo")))?;
    let app = macos.parent().filter(|_| macos.ends_with("Contents/MacOS"))?.parent()?;
    app.extension().is_some_and(|e| e == "app").then_some(app)
}

/// The `<string>` after `<key>key</key>` in an XML plist.
fn plist_string<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let rest = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = rest.find("<string>")? + "<string>".len();
    Some(rest[start..start + rest[start..].find("</string>")?].trim())
}

/// This Mac's server: who runs it (or why it can't be started), and
/// whether to suggest moving Silo to Applications.
pub fn local_service(home: &Path, server_exe: PathBuf) -> (Result<Service, String>, bool) {
    let app_exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
    let user_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let plist = bundle_of(&app_exe).and_then(|app| std::fs::read_to_string(app.join("Contents/Info.plist")).ok()).unwrap_or_default();
    let silo_home = std::env::var_os("SILO_HOME");
    let origin = Origin {
        silo_home: silo_home.as_deref(),
        release: !cfg!(debug_assertions),
        app_exe: &app_exe,
        server_exe: &server_exe,
        bundle_id: plist_string(&plist, "CFBundleIdentifier"),
        user_home: &user_home,
    };
    let launch = launch(&origin);
    let manager = if launch == Launch::Launchd { service::detect(&service::System) } else { Manager::None };
    tracing::info!(?launch, ?manager, "server launch");
    let service = Service::local(manager, server_exe, home.to_owned()).map_err(|e| {
        tracing::warn!("server: {e:#}");
        "Silo's server is missing".to_owned()
    });
    (service, launch == Launch::Unmoved)
}

/// `service::up` on a blocking thread (launchctl and the waits stay off
/// the network runtime); its notes.
async fn up(svc: &Service) -> anyhow::Result<Vec<String>> {
    let svc = svc.clone();
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(async { service::up(&svc, &service::System, &client_name()).await.map(|up| up.notes) })
    })
    .await?
}

/// Bring the server up as `svc` runs it: install or refresh the launchd
/// job, take over a server of another build live, adopt an unmanaged one,
/// or start a plain server. Failures are logged; the connection loop
/// reports what it then finds.
async fn ensure(svc: &Service) {
    match up(svc).await {
        Ok(notes) => notes.iter().for_each(|note| tracing::info!("service: {note}")),
        Err(error) => tracing::warn!("start server: {error:#}"),
    }
}

/// The server is gone and no process holds its state directory (Restart, a
/// crash): start it again. Under launchd only while the job is loaded, so
/// `silo down` stays down.
async fn revive(svc: &Service) {
    let svc = svc.clone();
    let revived = tokio::task::spawn_blocking(move || revive_with(&svc, &service::System)).await;
    match revived.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(Some(action)) => tracing::info!(?action, "server revived"),
        Ok(None) => tracing::info!("service not loaded or already starting; not starting the server"),
        Err(error) => tracing::warn!("start server: {error:#}"),
    }
}

/// [`revive`]'s step: a plain server, or the loaded launchd job if it is
/// not running (KeepAlive may already be restarting it; never `-k` that).
fn revive_with(svc: &Service, runner: &dyn service::Runner) -> anyhow::Result<Option<Action>> {
    let action = match svc.manager {
        Manager::None => Action::Spawn,
        _ => {
            let state = svc.probe(runner, service::Server::Absent)?;
            if !state.loaded || state.running {
                return Ok(None);
            }
            Action::Start
        }
    };
    svc.apply(runner, &[action])?;
    Ok(Some(action))
}

/// Replace the different-build server (see `mismatch::upgrade`), then
/// bring this build's up under `svc`.
///
/// # Errors
/// The old server could not be replaced or this build's started.
pub async fn upgrade(svc: &Service, server: &DifferentBuild) -> anyhow::Result<Upgraded> {
    let upgraded = silo_client::mismatch::upgrade(&svc.silo_home, &svc.exe, server).await?;
    tracing::info!(?upgraded, "upgraded server");
    up(svc).await?;
    Ok(upgraded)
}

/// A binary next to this executable (not this executable itself, which on a
/// case-insensitive volume may answer to another name).
#[must_use]
pub fn sibling(names: &[&str]) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent()?;
    names.iter().map(|name| dir.join(name)).find(|candidate| candidate.is_file() && candidate.canonicalize().ok().as_deref() != Some(exe.as_path()))
}

/// Start a host connection loop. While `paused` is set (Stop Server) the
/// loop neither connects nor starts a server.
pub fn start(rt: &tokio::runtime::Handle, transport: Transport, paused: Arc<AtomicBool>) -> mpsc::UnboundedReceiver<ConnEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    rt.spawn(run(transport, tx, paused));
    rx
}

/// The remote server's current route from `server.info`, if it has one.
async fn server_addr(control: &Control) -> Option<silo_client::transport::EndpointAddr> {
    let info: silo_proto::control::ServerInfo =
        control.call(silo_proto::control::Method::ServerInfo {}).await.inspect_err(|e| tracing::debug!("server.info: {e:#}")).ok()?;
    serde_json::from_value(info.endpoint_addr?).inspect_err(|e| tracing::debug!("server address: {e}")).ok()
}

async fn run(mut transport: Transport, tx: mpsc::UnboundedSender<ConnEvent>, paused: Arc<AtomicBool>) {
    let name = client_name();
    let remote = transport.is_remote();
    let mut attempt = 0u32;
    let mut remote_build = false;
    let mut incompatible: Option<DifferentBuild> = None;
    let mut ensured = false;
    loop {
        if paused.load(Ordering::Acquire) {
            if tx.is_closed() {
                return;
            }
            // Start Server after Stop: up again (Stop disabled the launchd job).
            ensured = false;
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        // Quit Silo stopped the incompatible server: don't start ours.
        if let Some(pid) = incompatible.as_ref().and_then(|s| s.pid)
            && quit_requested(&transport, pid)
        {
            tracing::info!(pid, "incompatible server stopped by Quit Silo");
            let _ = tx.send(ConnEvent::Quit);
            return;
        }
        if let Transport::Local { service: Ok(service), .. } = &transport
            && !std::mem::replace(&mut ensured, true)
        {
            ensure(service).await;
        }
        // A stopped server accepts the socket but never answers the hello.
        let limit = if remote { silo_client::PING_TIMEOUT_IROH } else { silo_client::PING_TIMEOUT_LOCAL };
        let connected = tokio::time::timeout(limit, transport.connect(&name)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("server not responding")));
        match connected {
            Ok((control, mut events)) => {
                attempt = 0;
                remote_build = false;
                incompatible = None;
                let pid = control.hello().server.pid;
                tracing::info!(pid, "connected to silo server");
                if let Transport::Local { home, .. } = &transport {
                    silo_proto::paths::clear_quit(home, Some(pid));
                }
                if tx.send(ConnEvent::Connected(control.clone())).is_err() {
                    return;
                }
                // Remote: read the server's current route now and every few
                // minutes, so a restart or port change stays reachable.
                let mut info = tokio::time::interval(ADDR_REFRESH);
                let heartbeat = control.clone().heartbeat();
                tokio::pin!(heartbeat);
                let mut hung = false;
                loop {
                    tokio::select! {
                        event = events.recv() => {
                            let Some((_, event)) = event else { break };
                            if tx.send(ConnEvent::Event(event)).is_err() {
                                return;
                            }
                        }
                        () = &mut heartbeat => {
                            tracing::warn!(pid, "server not responding");
                            hung = true;
                            break;
                        }
                        _ = info.tick(), if remote => {
                            if let Some(addr) = server_addr(&control).await
                                && let Transport::Iroh { addr: current, .. } = &mut transport
                                && *current != addr
                                && addr.id == current.id
                            {
                                current.clone_from(&addr);
                                if tx.send(ConnEvent::Addr(addr)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                if quit_requested(&transport, pid) {
                    tracing::info!(pid, "server stopped by Quit Silo");
                    let _ = tx.send(ConnEvent::Quit);
                    return;
                }
                let reason = if hung {
                    "server not responding"
                } else if paused.load(Ordering::Acquire) {
                    "server stopped"
                } else {
                    "connection closed"
                };
                if tx.send(ConnEvent::Disconnected(reason.into())).is_err() {
                    return;
                }
            }
            Err(error) => {
                if let Some(server) = error.downcast_ref::<DifferentBuild>() {
                    // Slow, quiet polling: nothing is spawned meanwhile.
                    if incompatible.as_ref() != Some(server) {
                        tracing::info!(?server, "incompatible server");
                        incompatible = Some(server.clone());
                        if tx.send(ConnEvent::DifferentBuild(server.clone())).is_err() {
                            return;
                        }
                    }
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(INCOMPATIBLE_POLL).await;
                    continue;
                }
                incompatible = None;
                tracing::debug!("connect failed: {error:#}");
                let delay = match classify(&error) {
                    Failure::Upgrading => UPGRADING_RETRY,
                    Failure::NotPaired => {
                        let _ = tx.send(ConnEvent::NotPaired);
                        return;
                    }
                    Failure::DifferentBuild => {
                        if !std::mem::replace(&mut remote_build, true) && tx.send(ConnEvent::RemoteBuild).is_err() {
                            return;
                        }
                        backoff(u32::MAX, remote, jitter())
                    }
                    Failure::Unreachable => {
                        remote_build = false;
                        let reason = match &transport {
                            Transport::Local { service: Err(reason), .. } => reason.clone(),
                            Transport::Local { home, service: Ok(service) } => {
                                if !silo_client::mismatch::server_lock_held(home) {
                                    revive(service).await;
                                }
                                "connection failed".into()
                            }
                            Transport::Iroh { .. } => "connection failed".into(),
                        };
                        // The full error (paths and all) stays in the log.
                        if tx.send(ConnEvent::Disconnected(reason)).is_err() {
                            return;
                        }
                        backoff(attempt, remote, jitter())
                    }
                };
                if tx.is_closed() {
                    return;
                }
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Whether the server with `pid` went away because of "Quit Silo" (as
/// opposed to a crash, idle exit, Stop Server or Restart Server).
fn quit_requested(transport: &Transport, pid: u32) -> bool {
    match transport {
        Transport::Local { home, .. } => silo_proto::paths::quit_marked(home, pid),
        Transport::Iroh { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str) -> (PathBuf, Transport) {
        let home = std::env::temp_dir().join(format!("silo-quit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        (home.clone(), crate::hosts::tests::local_transport(&home))
    }

    #[derive(Default)]
    struct Fake {
        calls: std::cell::RefCell<Vec<String>>,
        print: Option<&'static str>,
    }

    impl service::Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<service::Output> {
            self.calls.borrow_mut().push(std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" "));
            let print = program == "launchctl" && args.first() == Some(&"print");
            Ok(service::Output { success: !print || self.print.is_some(), stdout: self.print.unwrap_or_default().to_owned(), stderr: String::new() })
        }
    }

    #[test]
    fn launchd_only_for_an_installed_release_bundle() {
        let app = Path::new("/Applications/Silo.app/Contents/MacOS/Silo");
        let server = Path::new("/Applications/Silo.app/Contents/MacOS/silo-server");
        let home = Path::new("/Users/bat");
        let base = Origin { silo_home: None, release: true, app_exe: app, server_exe: server, bundle_id: Some(BUNDLE_ID), user_home: home };
        assert_eq!(launch(&base), Launch::Launchd);
        assert_eq!(launch(&Origin { silo_home: Some(OsStr::new("")), ..base }), Launch::Launchd);
        let mine = Path::new("/Users/bat/Applications/Silo.app/Contents/MacOS/Silo");
        let mine_server = Path::new("/Users/bat/Applications/Silo.app/Contents/MacOS/silo-server");
        assert_eq!(launch(&Origin { app_exe: mine, server_exe: mine_server, ..base }), Launch::Launchd);
        // Dev homes, debug builds, other bundles or servers: a plain server.
        assert_eq!(launch(&Origin { silo_home: Some(OsStr::new("/tmp/silo-ui")), ..base }), Launch::Plain);
        assert_eq!(launch(&Origin { release: false, ..base }), Launch::Plain);
        assert_eq!(launch(&Origin { bundle_id: Some("xyz.nitish.silo.dev"), ..base }), Launch::Plain);
        assert_eq!(launch(&Origin { bundle_id: None, ..base }), Launch::Plain);
        assert_eq!(launch(&Origin { server_exe: Path::new("/usr/local/bin/silo"), ..base }), Launch::Plain);
        let dev = Path::new("/w/target/debug/SiloDev.app/Contents/MacOS/SiloDev");
        assert_eq!(launch(&Origin { app_exe: dev, ..base }), Launch::Plain);
        assert_eq!(launch(&Origin { app_exe: Path::new("/w/target/release/silo-app"), ..base }), Launch::Plain);
        // Run from a disk image, Downloads or App Translocation: suggest moving it.
        for dir in ["/Volumes/Silo", "/Users/bat/Downloads", "/private/var/folders/x/AppTranslocation/1234/d", "/Applications/Utilities"] {
            let (exe, server) = (format!("{dir}/Silo.app/Contents/MacOS/Silo"), format!("{dir}/Silo.app/Contents/MacOS/silo-server"));
            assert_eq!(launch(&Origin { app_exe: Path::new(&exe), server_exe: Path::new(&server), ..base }), Launch::Unmoved, "{dir}");
        }
    }

    #[test]
    fn reads_bundle_id() {
        let plist =
            "<dict>\n  <key>CFBundleName</key><string>Silo</string>\n  <key>CFBundleIdentifier</key><string>xyz.nitish.silo</string>\n</dict>";
        assert_eq!(plist_string(plist, "CFBundleIdentifier"), Some("xyz.nitish.silo"));
        assert_eq!(plist_string(plist, "CFBundleVersion"), None);
        assert_eq!(plist_string("", "CFBundleIdentifier"), None);
    }

    fn plan(manager: Manager, home: &Path, runner: &Fake, server: service::Server) -> Vec<service::Action> {
        let svc =
            Service::new(manager, "/Applications/Silo.app/Contents/MacOS/silo-server".into(), home.join("state"), home.to_owned(), 501, "bat".into());
        service::plan_up(&svc.unwrap().probe(runner, server).unwrap())
    }

    #[test]
    fn revives_only_a_stopped_loaded_job() {
        let home = Path::new("/nonexistent/silo-revive");
        let svc = Service::new(
            Manager::Launchd,
            "/Applications/Silo.app/Contents/MacOS/silo-server".into(),
            home.join("s"),
            home.into(),
            501,
            "bat".into(),
        );
        let svc = svc.unwrap();
        // KeepAlive is restarting it: leave it alone.
        let running = Fake { print: Some("\tstate = running\n"), ..Fake::default() };
        assert_eq!(revive_with(&svc, &running).unwrap(), None);
        assert_eq!(*running.calls.borrow(), ["launchctl print gui/501/dev.silo.server"]);
        // `silo down` / Stop: not loaded, stays down.
        let unloaded = Fake::default();
        assert_eq!(revive_with(&svc, &unloaded).unwrap(), None);
        assert_eq!(unloaded.calls.borrow().len(), 1);
        // Loaded but not running: start it, without killing anything.
        let stopped = Fake { print: Some("\tstate = not running\n"), ..Fake::default() };
        assert_eq!(revive_with(&svc, &stopped).unwrap(), Some(Action::Start));
        assert_eq!(*stopped.calls.borrow(), ["launchctl print gui/501/dev.silo.server", "launchctl kickstart gui/501/dev.silo.server"]);
    }

    #[test]
    fn launch_plans() {
        use service::{Action::*, Server};
        let home = std::env::temp_dir().join(format!("silo-launch-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // A custom SILO_HOME never asks launchd.
        let fake = Fake::default();
        assert_eq!(plan(Manager::None, &home, &fake, Server::Absent), [Spawn]);
        assert_eq!(plan(Manager::None, &home, &fake, Server::Other), [Takeover]);
        assert_eq!(plan(Manager::None, &home, &fake, Server::Current), []);
        assert!(fake.calls.borrow().is_empty());
        // Default home: install the job, adopt or take over through it.
        let fake = Fake::default();
        assert_eq!(plan(Manager::Launchd, &home, &fake, Server::Absent), [WriteUnit, Enable, Bootstrap]);
        assert_eq!(plan(Manager::Launchd, &home, &fake, Server::Idle), [WriteUnit, Enable, Bootstrap]);
        assert_eq!(plan(Manager::Launchd, &home, &fake, Server::Other), [Takeover, WriteUnit, Enable, Bootstrap]);
        assert_eq!(fake.calls.borrow()[0], "launchctl print gui/501/dev.silo.server");
        let running = Fake { print: Some("\tstate = running\n"), ..Fake::default() };
        assert_eq!(plan(Manager::Launchd, &home, &running, Server::Other), [Takeover, WriteUnit, Bootout, Enable, Bootstrap]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn crash_or_restart_reconnects() {
        let (home, transport) = local("crash");
        assert!(!quit_requested(&transport, 42));
        // A marker for an older server (e.g. before a restart) is ignored.
        silo_proto::paths::mark_quit(&home, 7).unwrap();
        assert!(!quit_requested(&transport, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn quit_silo_quits() {
        let (home, transport) = local("quit");
        silo_proto::paths::mark_quit(&home, 42).unwrap();
        assert!(quit_requested(&transport, 42));
        // Connecting to another server clears the stale marker.
        silo_proto::paths::clear_quit(&home, Some(43));
        assert!(!quit_requested(&transport, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remote_never_quits() {
        let home = crate::hosts::tests::temp("never-quits");
        let addr = crate::hosts::tests::addr(&home.join("key"));
        silo_proto::paths::mark_quit(&home, 42).unwrap();
        assert!(!quit_requested(&Transport::Iroh { home: home.clone(), addr }, 42));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn backoff_schedule() {
        let ms = |attempt, remote, jitter| backoff(attempt, remote, jitter).as_millis();
        assert_eq!([0, 1, 2, 3, 5].map(|a| ms(a, false, 1.0)), [100, 200, 400, 800, 3000]);
        assert_eq!([0, 1, 5, 6, 40].map(|a| ms(a, true, 1.0)), [250, 500, 8000, 15000, 15000]);
        // Jitter scales to 50–100 % and never goes past the cap.
        assert_eq!(ms(0, true, 0.0), 125);
        assert_eq!(ms(u32::MAX, true, 0.0), 7500);
        assert_eq!(ms(3, true, 7.0), 2000);
    }

    #[test]
    fn errors_map_to_failures() {
        let message = String::from("why");
        assert_eq!(classify(&silo_client::Upgrading { message: message.clone() }.into()), Failure::Upgrading);
        assert_eq!(classify(&silo_client::Unauthorized { message: message.clone() }.into()), Failure::NotPaired);
        assert_eq!(classify(&silo_client::BuildMismatch { message }.into()), Failure::DifferentBuild);
        assert_eq!(classify(&anyhow::anyhow!("dial server")), Failure::Unreachable);
        // Context added on the way up keeps the type.
        let wrapped = anyhow::Error::from(silo_client::Unauthorized { message: "x".into() }).context("hello");
        assert_eq!(classify(&wrapped), Failure::NotPaired);
    }
}
