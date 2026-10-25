//! `silo up`/`down`: the server as a user service of the platform's service
//! manager (systemd user unit on Linux, launchd agent on macOS).

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use silo_proto::paths;

use crate::Control;

/// This channel's systemd unit: `silo.service`, dev `silo-dev.service`.
pub const UNIT: &str = silo_proto::CHANNEL.systemd_unit();
/// This channel's launchd job: `dev.silo.server`, dev `dev.silo.server.dev`.
pub const LABEL: &str = silo_proto::CHANNEL.launchd_label();

const SYSTEMD_TEMPLATE: &str = include_str!("../service/silo.service");
const LAUNCHD_TEMPLATE: &str = include_str!("../service/dev.silo.server.plist");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner {
    /// Run `program` to completion.
    ///
    /// # Errors
    /// The program cannot be started.
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Output>;
}

pub struct System;

impl Runner for System {
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Output> {
        let out = std::process::Command::new(program).args(args).output().with_context(|| format!("run {program}"))?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The service manager of this machine; `SILO_SERVICE_MANAGER=none` opts
/// out (e.g. in a container whose user manager should not own the server).
pub fn detect(runner: &dyn Runner) -> Manager {
    if std::env::var_os("SILO_SERVICE_MANAGER").is_some_and(|m| m == "none") {
        return Manager::None;
    }
    detect_os(std::env::consts::OS, runner)
}

fn detect_os(os: &str, runner: &dyn Runner) -> Manager {
    match os {
        "macos" => Manager::Launchd,
        "linux" if runner.run("systemctl", &["--user", "show-environment"]).is_ok_and(|o| o.success) => Manager::Systemd,
        _ => Manager::None,
    }
}

/// The running server, as seen on the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Server {
    Absent,
    /// Of this build, never exiting when idle.
    Current,
    /// Of this build, exiting when idle (as clients auto-start it).
    Idle,
    /// Of another build.
    Other,
    /// Holds the state directory but does not answer (starting, hung).
    Unresponsive,
    /// Of the other channel (a release server for a dev build, or the
    /// reverse): never taken over, adopted or stopped by this build.
    OtherChannel(silo_proto::Channel),
}

/// What the service manager and the socket report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub manager: Manager,
    /// The unit file on disk.
    pub installed: Option<String>,
    /// The unit file `up` writes.
    pub desired: Option<String>,
    /// systemd: enabled; launchd: bootstrapped.
    pub loaded: bool,
    /// The service's process is running.
    pub running: bool,
    pub server: Server,
    /// Linux only; `None` when unknown.
    pub linger: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    WriteUnit,
    RemoveUnit,
    DaemonReload,
    /// systemd: enable the unit; launchd: clear a `Disable`.
    Enable,
    /// systemd: start the unit; launchd: start the loaded job if it is not running (`kickstart`, no `-k`).
    Start,
    /// systemd `ExecReload`: a takeover inside the unit.
    Reload,
    Stop,
    /// systemd: disable the unit; launchd: keep it from loading at login.
    Disable,
    Bootstrap,
    Bootout,
    /// Restart the launchd job (`kickstart -k`).
    Kickstart,
    /// A takeover successor outside the service manager.
    Takeover,
    /// A server outside the service manager.
    Spawn,
    EnableLinger,
}

#[must_use]
pub fn plan_up(state: &State) -> Vec<Action> {
    let stale = state.installed != state.desired;
    let mut actions = Vec::new();
    match state.manager {
        Manager::Systemd => {
            if stale {
                actions.extend([Action::WriteUnit, Action::DaemonReload]);
            }
            if !state.loaded {
                actions.push(Action::Enable);
            }
            if !state.running {
                actions.push(Action::Start);
            } else if state.server == Server::Other {
                actions.push(Action::Reload);
            }
            if state.linger == Some(false) {
                actions.push(Action::EnableLinger);
            }
        }
        Manager::Launchd => {
            // launchd runs one process per label and bootout/kickstart -k SIGTERM it, which ends
            // every session: upgrade by a takeover outside launchd, then the job adopts it (two handoffs).
            if state.loaded && state.running && !stale && state.server != Server::Other {
                return actions;
            }
            if state.server == Server::Other || (state.loaded && state.running) {
                actions.push(Action::Takeover);
            }
            if stale {
                actions.push(Action::WriteUnit);
                if state.loaded {
                    actions.push(Action::Bootout);
                }
                actions.extend([Action::Enable, Action::Bootstrap]);
            } else if state.loaded {
                actions.push(Action::Kickstart);
            } else {
                actions.extend([Action::Enable, Action::Bootstrap]);
            }
        }
        Manager::None => match state.server {
            Server::Absent => actions.push(Action::Spawn),
            Server::Current | Server::Unresponsive | Server::OtherChannel(_) => {}
            Server::Idle | Server::Other => actions.push(Action::Takeover),
        },
    }
    actions
}

#[must_use]
pub fn plan_down(state: &State) -> Vec<Action> {
    let mut actions = Vec::new();
    match state.manager {
        Manager::Systemd => {
            if state.running {
                actions.push(Action::Stop);
            }
            if state.loaded {
                actions.push(Action::Disable);
            }
            if state.installed.is_some() {
                actions.extend([Action::RemoveUnit, Action::DaemonReload]);
            }
        }
        Manager::Launchd => {
            if state.loaded {
                actions.push(Action::Bootout);
            }
            if state.installed.is_some() {
                actions.push(Action::RemoveUnit);
            }
        }
        Manager::None => {}
    }
    actions
}

/// The silo service of one user.
#[derive(Clone, Debug)]
pub struct Service {
    pub manager: Manager,
    /// The absolute path of the `silo` binary the service runs.
    pub exe: PathBuf,
    /// `SILO_HOME` pinned in the service.
    pub silo_home: PathBuf,
    pub user_home: PathBuf,
    pub uid: u32,
    pub user: String,
}

impl Service {
    /// # Errors
    /// `exe` is relative.
    pub fn new(manager: Manager, exe: PathBuf, silo_home: PathBuf, user_home: PathBuf, uid: u32, user: String) -> anyhow::Result<Self> {
        if !exe.is_absolute() {
            bail!("service binary path is not absolute: {}", exe.display());
        }
        Ok(Self { manager, exe, silo_home, user_home, uid, user })
    }

    /// The service of this user running `exe` for `silo_home`.
    ///
    /// # Errors
    /// `exe` is relative or `HOME` is not set.
    pub fn local(manager: Manager, exe: PathBuf, silo_home: PathBuf) -> anyhow::Result<Self> {
        let user_home = std::env::var_os("HOME").filter(|h| !h.is_empty()).context("HOME is not set")?.into();
        let user = std::env::var("USER").unwrap_or_default();
        Self::new(manager, exe, silo_home, user_home, rustix::process::getuid().as_raw(), user)
    }

    #[must_use]
    pub fn unit_path(&self) -> Option<PathBuf> {
        match self.manager {
            Manager::Systemd => Some(self.user_home.join(".config/systemd/user").join(UNIT)),
            Manager::Launchd => Some(self.user_home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))),
            Manager::None => None,
        }
    }

    #[must_use]
    pub fn unit_text(&self) -> Option<String> {
        match self.manager {
            Manager::Systemd => Some(systemd_unit(&self.exe, &self.silo_home)),
            Manager::Launchd => Some(launchd_plist(&self.exe, &self.silo_home)),
            Manager::None => None,
        }
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }

    /// The pid of the launchd job's process, if it runs (always `None` for
    /// other managers).
    ///
    /// # Errors
    /// `launchctl` cannot be run.
    pub fn job_pid(&self, runner: &dyn Runner) -> anyhow::Result<Option<u32>> {
        if self.manager != Manager::Launchd {
            return Ok(None);
        }
        let print = runner.run("launchctl", &["print", &self.target()])?;
        Ok(if print.success { launchd_pid(&print.stdout) } else { None })
    }

    /// # Errors
    /// A service manager query cannot be run.
    pub fn probe(&self, runner: &dyn Runner, server: Server) -> anyhow::Result<State> {
        let installed = self.unit_path().and_then(|path| std::fs::read_to_string(path).ok());
        let (loaded, running, linger) = match self.manager {
            Manager::Systemd => {
                let loaded = runner.run("systemctl", &["--user", "is-enabled", UNIT])?.success;
                let running = runner.run("systemctl", &["--user", "is-active", UNIT])?.success;
                let linger = runner.run("loginctl", &["show-user", &self.user, "-p", "Linger"])?;
                (loaded, running, linger.success.then(|| linger.stdout.trim() == "Linger=yes"))
            }
            Manager::Launchd => {
                let print = runner.run("launchctl", &["print", &self.target()])?;
                (print.success, print.success && print.stdout.contains("state = running"), None)
            }
            Manager::None => (false, false, None),
        };
        Ok(State { manager: self.manager, installed, desired: self.unit_text(), loaded, running, server, linger })
    }

    /// Apply `actions` in order; returns notes for the user.
    ///
    /// # Errors
    /// An action fails; the ones before it stay applied.
    pub fn apply(&self, runner: &dyn Runner, actions: &[Action]) -> anyhow::Result<Vec<String>> {
        let mut notes = Vec::new();
        let unit = self.unit_path().unwrap_or_default();
        let unit_str = unit.to_string_lossy().into_owned();
        let exe = self.exe.to_string_lossy().into_owned();
        for action in actions {
            match action {
                Action::WriteUnit => {
                    let text = self.unit_text().context("no service manager to write a unit for")?;
                    let dir = unit.parent().context("unit path has no parent")?;
                    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
                    std::fs::write(&unit, text).with_context(|| format!("write {}", unit.display()))?;
                    tracing::info!("service: wrote {}", unit.display());
                }
                Action::RemoveUnit => {
                    std::fs::remove_file(&unit).with_context(|| format!("remove {}", unit.display()))?;
                    tracing::info!("service: removed {}", unit.display());
                }
                Action::DaemonReload => check(runner, "systemctl", &["--user", "daemon-reload"])?,
                Action::Enable if self.manager == Manager::Launchd => check(runner, "launchctl", &["enable", &self.target()])?,
                Action::Disable if self.manager == Manager::Launchd => check(runner, "launchctl", &["disable", &self.target()])?,
                Action::Start if self.manager == Manager::Launchd => check(runner, "launchctl", &["kickstart", &self.target()])?,
                Action::Enable => check(runner, "systemctl", &["--user", "enable", UNIT])?,
                Action::Start => check(runner, "systemctl", &["--user", "start", UNIT])?,
                Action::Reload => check(runner, "systemctl", &["--user", "reload", UNIT])?,
                Action::Stop => check(runner, "systemctl", &["--user", "stop", UNIT])?,
                Action::Disable => check(runner, "systemctl", &["--user", "disable", UNIT])?,
                Action::Bootstrap => check(runner, "launchctl", &["bootstrap", &self.domain(), &unit_str])?,
                Action::Bootout => check(runner, "launchctl", &["bootout", &self.target()])?,
                Action::Kickstart => check(runner, "launchctl", &["kickstart", "-k", &self.target()])?,
                Action::Takeover => check(runner, &exe, &["server", "--takeover", "--daemon", "--idle-exit", "0"])?,
                Action::Spawn => check(runner, &exe, &["server", "--daemon", "--idle-exit", "0"])?,
                Action::EnableLinger => {
                    if !runner.run("loginctl", &["enable-linger", &self.user])?.success {
                        notes.push(format!("the server stops when you log out; keep it running with `sudo loginctl enable-linger {}`", self.user));
                    }
                }
            }
        }
        Ok(notes)
    }
}

/// The job's `pid = N` in `launchctl print` output.
fn launchd_pid(print: &str) -> Option<u32> {
    print.lines().find_map(|line| line.trim().strip_prefix("pid = ")?.trim().parse().ok())
}

/// The server for `home`, if one of this build answers, and how it runs. A
/// compatible server of another build answers too, but is `Other`: it is
/// taken over to this build.
pub async fn probe_server(home: &Path, client_name: &str) -> (Server, Option<Control>) {
    match Control::connect(&paths::socket(home), client_name).await {
        Ok((control, _)) if control.hello().channel != silo_proto::CHANNEL => (Server::OtherChannel(control.hello().channel), None),
        Ok((control, _)) if control.hello().build != silo_proto::BUILD => (Server::Other, None),
        Ok((control, _)) if control.hello().server.idle_exit_secs == 0 => (Server::Current, Some(control)),
        Ok((control, _)) => (Server::Idle, Some(control)),
        Err(error) if crate::mismatch::incompatible_server(home, &error).await.is_some() => match paths::check_runtime_channel(home) {
            Err(other) => (Server::OtherChannel(other.server), None),
            Ok(()) => (Server::Other, None),
        },
        Err(_) if crate::mismatch::server_lock_held(home) => (Server::Unresponsive, None),
        Err(_) => (Server::Absent, None),
    }
}

/// A server of another build still holds the state directory after `up`:
/// it predates live upgrade and keeps its sessions until stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotUpgraded;

impl std::fmt::Display for NotUpgraded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the running server of another build cannot be upgraded live")
    }
}

impl std::error::Error for NotUpgraded {}

/// What [`up`] did.
pub struct Up {
    pub control: Control,
    pub actions: Vec<Action>,
    pub notes: Vec<String>,
}

/// Install or update the service and start it; returns once this build's
/// server answers (under launchd: the job's own process, not a takeover
/// successor serving before the job adopts it).
///
/// # Errors
/// An action fails, the server does not come up, [`NotUpgraded`], or
/// [`silo_proto::OtherChannel`] (nothing is done to that server).
pub async fn up(svc: &Service, runner: &dyn Runner, client_name: &str) -> anyhow::Result<Up> {
    let home = &svc.silo_home;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (server, control) = loop {
        match probe_server(home, client_name).await {
            (Server::Unresponsive, _) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
            (Server::Unresponsive, _) => bail!("the server is running but not responding"),
            (Server::OtherChannel(server), _) => return Err(silo_proto::OtherChannel { server }.into()),
            probed => break probed,
        }
    };
    drop(control);
    let state = svc.probe(runner, server)?;
    let actions = plan_up(&state);
    tracing::info!(manager = ?svc.manager, ?server, loaded = state.loaded, running = state.running, stale = state.installed != state.desired, ?actions, "service up");
    let notes = match svc.apply(runner, &actions) {
        Ok(notes) => notes,
        Err(error) if probe_server(home, client_name).await.0 == Server::Other => {
            tracing::warn!("service up: {error:#}");
            return Err(NotUpgraded.into());
        }
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let (Server::Current, Some(control)) = probe_server(home, client_name).await
            && (svc.manager != Manager::Launchd || svc.job_pid(runner)? == Some(control.hello().server.pid))
        {
            tracing::info!(pid = control.hello().server.pid, manager = ?svc.manager, "service server up");
            return Ok(Up { control, actions, notes });
        }
        if Instant::now() >= deadline {
            if probe_server(home, client_name).await.0 == Server::Other {
                return Err(NotUpgraded.into());
            }
            bail!("the server did not come up; see the server log");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn check(runner: &dyn Runner, program: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = runner.run(program, args)?;
    tracing::info!(success = out.success, "service: {program} {}", args.join(" "));
    if !out.success {
        bail!("{program} {} failed: {}", args.join(" "), out.stderr.trim());
    }
    Ok(())
}

/// The systemd user unit running `exe` with `SILO_HOME=silo_home`.
#[must_use]
pub fn systemd_unit(exe: &Path, silo_home: &Path) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"));
    SYSTEMD_TEMPLATE.replace("@EXE@", &quote(&exe.to_string_lossy())).replace("@SILO_HOME@", &quote(&format!("SILO_HOME={}", silo_home.display())))
}

/// The launchd agent running `exe` with `SILO_HOME=silo_home`.
#[must_use]
pub fn launchd_plist(exe: &Path, silo_home: &Path) -> String {
    let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    LAUNCHD_TEMPLATE
        .replace("@LABEL@", LABEL)
        .replace("@EXE@", &xml(&exe.to_string_lossy()))
        .replace("@SILO_HOME@", &xml(&silo_home.to_string_lossy()))
        .replace("@LOG@", &xml(&silo_proto::paths::log(silo_home).to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        failing: Vec<&'static str>,
        stdout: Vec<(&'static str, &'static str)>,
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<Output> {
            let call = std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ");
            self.calls.borrow_mut().push(call.clone());
            let stdout = self.stdout.iter().find(|(p, _)| call.starts_with(p)).map_or("", |(_, s)| s);
            Ok(Output { success: !self.failing.iter().any(|p| call.starts_with(p)), stdout: stdout.to_owned(), stderr: "boom".to_owned() })
        }
    }

    impl Fake {
        /// The calls, with this channel's job names written as the release
        /// build's (`dev.silo.server`, `silo.service`) so one table covers both.
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|c| as_release(c)).collect()
        }
    }

    fn as_release(text: &str) -> String {
        text.replace(LABEL, "dev.silo.server").replace(UNIT, "silo.service")
    }

    #[test]
    fn job_names_follow_the_channel() {
        let (label, unit) = match silo_proto::CHANNEL {
            silo_proto::Channel::Release => ("dev.silo.server", "silo.service"),
            silo_proto::Channel::Dev => ("dev.silo.server.dev", "silo-dev.service"),
        };
        assert_eq!((LABEL, UNIT), (label, unit));
    }

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-service-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(manager: Manager, home: &Path) -> Service {
        Service::new(manager, "/opt/silo/bin/silo".into(), home.join("state"), home.to_owned(), 501, "bat".to_owned()).unwrap()
    }

    fn state(manager: Manager) -> State {
        State {
            manager,
            installed: Some("unit".to_owned()),
            desired: Some("unit".to_owned()),
            loaded: true,
            running: true,
            server: Server::Current,
            linger: Some(true),
        }
    }

    #[test]
    fn detects_manager() {
        assert_eq!(detect_os("macos", &Fake::default()), Manager::Launchd);
        let fake = Fake::default();
        assert_eq!(detect_os("linux", &fake), Manager::Systemd);
        assert_eq!(fake.calls(), ["systemctl --user show-environment"]);
        assert_eq!(detect_os("linux", &Fake { failing: vec!["systemctl"], ..Fake::default() }), Manager::None);
        assert_eq!(detect_os("freebsd", &Fake::default()), Manager::None);
    }

    #[test]
    fn rejects_relative_exe() {
        let home = PathBuf::from("/home/bat");
        assert!(Service::new(Manager::Systemd, "silo".into(), home.clone(), home, 1, "bat".into()).is_err());
    }

    #[test]
    fn systemd_unit_pins_exe_and_home() {
        let unit = systemd_unit(Path::new("/opt/my silo/silo"), Path::new("/home/bat/.local/state/silo"));
        assert!(unit.contains("ExecStart=\"/opt/my silo/silo\" server --adopt\n"));
        assert!(unit.contains("ExecReload=\"/opt/my silo/silo\" server --takeover --daemon --idle-exit 0\n"));
        assert!(unit.contains("Environment=\"SILO_HOME=/home/bat/.local/state/silo\"\n"));
        assert!(unit.contains("Type=notify\n") && unit.contains("KillMode=process\n"));
        assert!(!unit.contains('@'));
        assert!(systemd_unit(Path::new("/a%b/silo"), Path::new("/h")).contains("\"/a%%b/silo\""));
    }

    #[test]
    fn launchd_plist_pins_exe_home_and_log() {
        let plist = launchd_plist(Path::new("/Apps/S&o/silo"), Path::new("/Users/bat/Library/Application Support/silo"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains("<string>/Apps/S&amp;o/silo</string>\n\t\t<string>server</string>"));
        assert!(plist.contains("<key>SILO_HOME</key>\n\t\t<string>/Users/bat/Library/Application Support/silo</string>"));
        assert!(plist.contains("<string>/Users/bat/Library/Application Support/silo/server.log</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(!plist.contains('@'));
    }

    #[test]
    fn unit_paths_derive_from_home() {
        let home = Path::new("/home/bat");
        assert_eq!(service(Manager::Systemd, home).unit_path(), Some(home.join(".config/systemd/user").join(UNIT)));
        assert_eq!(service(Manager::Launchd, home).unit_path(), Some(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))));
    }

    #[test]
    fn systemd_up_plans() {
        use Action::*;
        let s = state(Manager::Systemd);
        assert_eq!(plan_up(&s), []);
        assert_eq!(
            plan_up(&State { installed: None, loaded: false, running: false, server: Server::Absent, ..s.clone() }),
            [WriteUnit, DaemonReload, Enable, Start]
        );
        assert_eq!(plan_up(&State { installed: Some("old".into()), ..s.clone() }), [WriteUnit, DaemonReload]);
        assert_eq!(plan_up(&State { server: Server::Other, ..s.clone() }), [Reload]);
        assert_eq!(plan_up(&State { installed: Some("old".into()), server: Server::Other, ..s.clone() }), [WriteUnit, DaemonReload, Reload]);
        assert_eq!(plan_up(&State { running: false, server: Server::Other, ..s.clone() }), [Start]);
        assert_eq!(plan_up(&State { loaded: false, running: false, ..s.clone() }), [Enable, Start]);
        assert_eq!(plan_up(&State { linger: Some(false), ..s.clone() }), [EnableLinger]);
        assert_eq!(plan_up(&State { linger: None, ..s }), []);
    }

    #[test]
    fn launchd_up_plans() {
        use Action::*;
        let s = State { linger: None, ..state(Manager::Launchd) };
        assert_eq!(plan_up(&s), []);
        assert_eq!(
            plan_up(&State { installed: None, loaded: false, running: false, server: Server::Absent, ..s.clone() }),
            [WriteUnit, Enable, Bootstrap]
        );
        assert_eq!(plan_up(&State { loaded: false, running: false, server: Server::Other, ..s.clone() }), [Takeover, Enable, Bootstrap]);
        assert_eq!(plan_up(&State { loaded: false, running: false, server: Server::Idle, ..s.clone() }), [Enable, Bootstrap]);
        assert_eq!(plan_up(&State { server: Server::Other, ..s.clone() }), [Takeover, Kickstart]);
        assert_eq!(plan_up(&State { server: Server::Unresponsive, ..s.clone() }), []);
        assert_eq!(plan_up(&State { installed: Some("old".into()), ..s.clone() }), [Takeover, WriteUnit, Bootout, Enable, Bootstrap]);
        assert_eq!(plan_up(&State { running: false, ..s.clone() }), [Kickstart]);
        assert_eq!(plan_up(&State { installed: Some("old".into()), running: false, ..s }), [WriteUnit, Bootout, Enable, Bootstrap]);
    }

    #[test]
    fn unmanaged_up_plans() {
        let s = State { installed: None, desired: None, loaded: false, running: false, linger: None, ..state(Manager::None) };
        assert_eq!(plan_up(&State { server: Server::Absent, ..s.clone() }), [Action::Spawn]);
        assert_eq!(plan_up(&State { server: Server::Other, ..s.clone() }), [Action::Takeover]);
        assert_eq!(plan_up(&State { server: Server::Idle, ..s.clone() }), [Action::Takeover]);
        assert_eq!(plan_up(&State { server: Server::Unresponsive, ..s.clone() }), []);
        assert_eq!(plan_up(&s), []);
    }

    #[test]
    fn down_plans() {
        use Action::*;
        assert_eq!(plan_down(&state(Manager::Systemd)), [Stop, Disable, RemoveUnit, DaemonReload]);
        assert_eq!(plan_down(&State { running: false, loaded: false, installed: None, ..state(Manager::Systemd) }), []);
        assert_eq!(plan_down(&state(Manager::Launchd)), [Bootout, RemoveUnit]);
        assert_eq!(plan_down(&State { loaded: false, installed: None, ..state(Manager::Launchd) }), []);
        assert_eq!(plan_down(&State { installed: None, desired: None, ..state(Manager::None) }), []);
        assert_eq!(service(Manager::None, Path::new("/h")).unit_path(), None);
    }

    #[test]
    fn probes_systemd() {
        let home = temp_home("probe-systemd");
        let svc = service(Manager::Systemd, &home);
        let fake = Fake { failing: vec!["systemctl --user is-active"], stdout: vec![("loginctl", "Linger=no\n")], ..Fake::default() };
        let st = svc.probe(&fake, Server::Other).unwrap();
        assert_eq!(
            st,
            State {
                manager: Manager::Systemd,
                installed: None,
                desired: svc.unit_text(),
                loaded: true,
                running: false,
                server: Server::Other,
                linger: Some(false),
            }
        );
        assert_eq!(
            fake.calls(),
            ["systemctl --user is-enabled silo.service", "systemctl --user is-active silo.service", "loginctl show-user bat -p Linger"]
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn probes_launchd() {
        let home = temp_home("probe-launchd");
        let svc = service(Manager::Launchd, &home);
        std::fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        std::fs::write(svc.unit_path().unwrap(), "old").unwrap();
        let fake = Fake { stdout: vec![("launchctl print", "\tstate = running\n")], ..Fake::default() };
        let st = svc.probe(&fake, Server::Current).unwrap();
        assert_eq!((st.installed.as_deref(), st.loaded, st.running, st.linger), (Some("old"), true, true, None));
        assert_eq!(fake.calls(), ["launchctl print gui/501/dev.silo.server"]);
        let st = svc.probe(&Fake { failing: vec!["launchctl"], ..Fake::default() }, Server::Absent).unwrap();
        assert!(!st.loaded && !st.running);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn launchd_job_pid() {
        let print = "gui/501/dev.silo.server = {\n\tactive count = 1\n\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n}\n";
        assert_eq!(launchd_pid(print), Some(4242));
        assert_eq!(launchd_pid("state = not running\n"), None);
        let home = Path::new("/Users/bat");
        let fake = Fake { stdout: vec![("launchctl print", "\tpid = 7\n")], ..Fake::default() };
        assert_eq!(service(Manager::Launchd, home).job_pid(&fake).unwrap(), Some(7));
        assert_eq!(service(Manager::Launchd, home).job_pid(&Fake { failing: vec!["launchctl"], ..Fake::default() }).unwrap(), None);
        let fake = Fake::default();
        assert_eq!(service(Manager::Systemd, home).job_pid(&fake).unwrap(), None);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn applies_systemd_actions() {
        use Action::*;
        let home = temp_home("apply-systemd");
        let svc = service(Manager::Systemd, &home);
        let fake = Fake { failing: vec!["loginctl"], ..Fake::default() };
        let notes = svc.apply(&fake, &[WriteUnit, DaemonReload, Enable, Start, Reload, EnableLinger]).unwrap();
        assert_eq!(std::fs::read_to_string(svc.unit_path().unwrap()).unwrap(), svc.unit_text().unwrap());
        assert_eq!(notes, ["the server stops when you log out; keep it running with `sudo loginctl enable-linger bat`"]);
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable silo.service",
                "systemctl --user start silo.service",
                "systemctl --user reload silo.service",
                "loginctl enable-linger bat"
            ]
        );
        let fake = Fake::default();
        assert!(svc.apply(&fake, &[EnableLinger]).unwrap().is_empty());
        svc.apply(&fake, &[Stop, Disable, RemoveUnit, DaemonReload]).unwrap();
        assert!(!svc.unit_path().unwrap().exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn applies_launchd_actions() {
        use Action::*;
        let home = temp_home("apply-launchd");
        let svc = service(Manager::Launchd, &home);
        let fake = Fake::default();
        svc.apply(&fake, &[Takeover, WriteUnit, Bootout, Enable, Bootstrap, Kickstart, Disable]).unwrap();
        let plist = svc.unit_path().unwrap();
        assert_eq!(std::fs::read_to_string(&plist).unwrap(), svc.unit_text().unwrap());
        assert_eq!(
            fake.calls(),
            [
                "/opt/silo/bin/silo server --takeover --daemon --idle-exit 0".to_owned(),
                "launchctl bootout gui/501/dev.silo.server".to_owned(),
                "launchctl enable gui/501/dev.silo.server".to_owned(),
                as_release(&format!("launchctl bootstrap gui/501 {}", plist.display())),
                "launchctl kickstart -k gui/501/dev.silo.server".to_owned(),
                "launchctl disable gui/501/dev.silo.server".to_owned(),
            ]
        );
        svc.apply(&fake, &[RemoveUnit]).unwrap();
        assert!(!plist.exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn failed_action_stops_apply() {
        let home = temp_home("apply-fail");
        let svc = service(Manager::Systemd, &home);
        let fake = Fake { failing: vec!["systemctl --user start"], ..Fake::default() };
        let error = svc.apply(&fake, &[Action::Start, Action::Reload]).unwrap_err();
        assert_eq!(as_release(&error.to_string()), "systemctl --user start silo.service failed: boom");
        assert_eq!(fake.calls(), ["systemctl --user start silo.service"]);
        let _ = std::fs::remove_dir_all(home);
    }
}
