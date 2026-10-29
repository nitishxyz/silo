//! The process's updater (see `silo_client::update` for the steps): checks at
//! launch and every few hours when "Check for updates automatically" is
//! on, or now for "Check for Updates"; downloads and stages a newer
//! release in the background and, once it is ready, offers it as the
//! titlebar's Update pill. Only a release Silo.app in (~)/Applications
//! updates; other builds never check. Once one is ready nothing is
//! checked again: a later release is found after the restart.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use gpui::{App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, Task};

use silo_client::update::{self, Failure, Feed, Phase};

use crate::{hosts_state::HostsState, status::Status};

/// First automatic check after launch.
const FIRST_CHECK: Duration = Duration::from_secs(15);
/// How often whether a check is due is looked at; by the wall clock, so
/// a Mac waking from sleep checks soon after.
const TICK: Duration = Duration::from_secs(30 * 60);
/// Time between automatic checks.
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Least time between two progress updates of the pill.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// A newer release is being downloaded and checked.
    Downloading {
        version: String,
        phase: Phase,
    },
    Ready {
        version: String,
        staged: PathBuf,
    },
    /// This copy of Silo cannot be replaced (`reason`, for the tooltip).
    Blocked {
        version: String,
        reason: &'static str,
    },
    Installing,
}

/// A check's outcome for the window that asked for it.
pub struct UpdateStatus {
    pub requester: EntityId,
    pub status: Status,
}

pub struct Updater {
    rt: tokio::runtime::Handle,
    /// The bundle to replace; `None`: this build does not update.
    app: Option<PathBuf>,
    /// How `app` is signed (known whenever `app` is).
    running: Option<update::Signature>,
    /// Why this build doesn't update, when it doesn't (shown by Check for Updates).
    off: Option<&'static str>,
    feed: Feed,
    state: State,
    last_check: Option<SystemTime>,
    /// The windows waiting for the running check's outcome.
    requesters: Vec<EntityId>,
    hosts: Entity<HostsState>,
    check: Option<Task<()>>,
    _tick: Task<()>,
}

impl EventEmitter<UpdateStatus> for Updater {}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

const UNWRITABLE: &str = "Silo can't replace itself in this folder";

pub fn ready(version: &str) -> String {
    format!("Silo {version} is ready: restart to update")
}

/// How the titlebar's Update pill looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillKind {
    /// Accent-filled; a click installs and restarts.
    Ready,
    /// Outlined, with a spinner.
    Busy,
    /// Outlined, with a spinner and a progress ring; the percent follows
    /// the ring in a slot as wide as `100%`.
    Progress(u8),
    /// Outlined.
    Blocked,
}

/// The Update pill for `state` (label, tooltip, kind); `None`: no pill.
pub fn pill(state: &State) -> Option<(String, String, PillKind)> {
    Some(match state {
        State::Idle => return None,
        State::Downloading { version, phase: Phase::Download { done: 0, total: None } } => {
            (format!("Downloading {version}"), format!("Downloading Silo {version}"), PillKind::Busy)
        }
        State::Downloading { version, phase: Phase::Download { done, total: None } } => {
            let mb = update::megabytes(*done);
            (format!("Downloading {version} · {mb} MB"), format!("Downloading Silo {version} ({mb} MB)"), PillKind::Busy)
        }
        State::Downloading { version, phase: Phase::Download { done, total: Some(total) } } => (
            format!("Downloading {version}"),
            format!("Downloading Silo {version} ({} of {} MB)", update::megabytes(*done), update::megabytes(*total)),
            PillKind::Progress(update::percent(*done, *total)),
        ),
        State::Downloading { version, phase: Phase::Verify } => {
            (format!("Verifying {version}…"), format!("Verifying Silo {version}"), PillKind::Busy)
        }
        State::Installing => ("Updating…".into(), "Installing and restarting".into(), PillKind::Busy),
        State::Ready { version, .. } => ("Update".into(), ready(version), PillKind::Ready),
        State::Blocked { version, reason } => ("Can't Update".into(), format!("Silo {version} is available. {reason}."), PillKind::Blocked),
    })
}

/// A local build: ad-hoc signed, so no update can be checked against its signature.
const OFF_AD_HOC: &str =
    "This is a local build (ad-hoc signed) and can't update itself. Install Silo from github.com/nitishxyz/silo/releases to get updates";
/// Not running from an installed Silo.app (a development run).
const OFF_NOT_INSTALLED: &str =
    "This Silo isn't an installed app and can't update itself. Install Silo from github.com/nitishxyz/silo/releases to get updates";

/// Whether an automatic check is due at `now` after one at `last`.
fn due(last: Option<SystemTime>, now: SystemTime) -> bool {
    last.is_none_or(|last| now.duration_since(last).map_or(true, |since| since >= INTERVAL))
}

impl Updater {
    pub fn install(rt: tokio::runtime::Handle, hosts: Entity<HostsState>, server_exe: &Path, cx: &mut App) {
        let (app, feed, test) = match Feed::from_env() {
            Some((feed, test)) => (crate::conn::updatable_app(server_exe, test), feed, test),
            None => (None, Feed::github(), false),
        };
        // How this app is signed, once: what every update must match.
        let running =
            app.as_ref().and_then(|app| rt.block_on(update::signature(app)).inspect_err(|error| tracing::warn!("update: this app: {error}")).ok());
        let off = match (&app, &running) {
            (None, _) => Some(OFF_NOT_INSTALLED),
            (Some(_), Some(running)) if !updates_allowed(&running.1, test) => Some(OFF_AD_HOC),
            (Some(_), None) => Some(OFF_NOT_INSTALLED),
            _ => None,
        };
        let app = app.filter(|_| off.is_none());
        tracing::info!(app = ?app, signed = ?running.as_ref().map(|r| &r.1), "updates");
        if let Some(app) = app.clone() {
            rt.spawn_blocking(move || update::clean_stale(&app));
        }
        // `silo update` replaced the app: restart into it.
        let (relaunch_tx, mut relaunch) = tokio::sync::mpsc::unbounded_channel();
        if app.is_some()
            && let Err(error) = update::on_relaunch(move || {
                let _ = relaunch_tx.send(());
            })
        {
            tracing::warn!("update: {error}");
        }
        let updater = cx.new(|cx| {
            cx.spawn(async move |this, cx| {
                while relaunch.recv().await.is_some() {
                    if this.update(cx, |this: &mut Self, cx| this.relaunch_if_replaced(cx)).is_err() {
                        return;
                    }
                }
            })
            .detach();
            let tick = if app.is_some() {
                cx.spawn(async move |this, cx| {
                    let mut wait = FIRST_CHECK;
                    loop {
                        cx.background_executor().timer(wait).await;
                        wait = TICK;
                        if this.update(cx, |this: &mut Self, cx| this.tick(cx)).is_err() {
                            return;
                        }
                    }
                })
            } else {
                Task::ready(())
            };
            Self { rt, app, running, off, feed, state: State::Idle, last_check: None, requesters: Vec::new(), hosts, check: None, _tick: tick }
        });
        cx.set_global(GlobalUpdater(updater));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalUpdater>().0.clone()
    }

    /// Whether this build updates at all.
    pub fn enabled(&self) -> bool {
        self.app.is_some()
    }

    /// Why this build doesn't update itself; `None` when it does.
    pub fn off(&self) -> Option<&'static str> {
        self.off.filter(|_| self.app.is_none())
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.hosts.read(cx).app_state().check_updates && due(self.last_check, SystemTime::now()) {
            self.check(None, cx);
        }
    }

    fn tell(&mut self, status: Status, cx: &mut Context<Self>) {
        for &requester in &self.requesters {
            cx.emit(UpdateStatus { requester, status: status.clone() });
        }
    }

    /// Look for a newer release now; `requester` (a window) is told how it
    /// goes. Without one (the automatic checks) failures are only logged.
    pub fn check(&mut self, requester: Option<EntityId>, cx: &mut Context<Self>) {
        let (Some(app), Some(running)) = (self.app.clone(), self.running.clone()) else { return };
        if let Some(requester) = requester.filter(|r| !self.requesters.contains(r)) {
            self.requesters.push(requester);
        }
        if self.check.is_some() {
            let status = match &self.state {
                State::Downloading { version, .. } => Status::progress(format!("Downloading Silo {version}…")),
                _ => Status::progress("Checking for updates…"),
            };
            return self.tell(status, cx);
        }
        if let State::Ready { version, .. } = &self.state {
            let status = Status::success(ready(version));
            self.tell(status, cx);
            self.requesters.clear();
            return;
        }
        if self.state == State::Installing {
            self.requesters.clear();
            return;
        }
        self.tell(Status::progress("Checking for updates…"), cx);
        self.last_check = Some(SystemTime::now());
        let (rt, feed) = (self.rt.clone(), self.feed.clone());
        self.check = Some(cx.spawn(async move |this, cx| {
            let latest = {
                let feed = feed.clone();
                rt.spawn(async move { update::check_latest(&feed, VERSION).await }).await
            };
            let version = match latest.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(Some(version)) => version,
                Ok(None) => return this.update(cx, |this, cx| this.done(None, Status::success("Silo is up to date"), cx)).unwrap_or(()),
                Err(error) => {
                    tracing::warn!("update check: {error}");
                    return this.update(cx, |this, cx| this.done(None, Status::error("Couldn't check for updates"), cx)).unwrap_or(());
                }
            };
            tracing::info!(version, "update: newer release, downloading");
            let (progress, mut phases) = tokio::sync::watch::channel(Phase::Download { done: 0, total: None });
            let _ = this.update(cx, |this, cx| {
                this.state = State::Downloading { version: version.clone(), phase: *phases.borrow() };
                this.tell(Status::progress(format!("Downloading Silo {version}…")), cx);
                // Until `prepare` drops `progress`; at most every PROGRESS_EVERY.
                let downloading = version.clone();
                cx.spawn(async move |this, cx| {
                    while phases.changed().await.is_ok() {
                        let phase = *phases.borrow_and_update();
                        if this.update(cx, |this, cx| this.progress(&downloading, phase, cx)).is_err() {
                            return;
                        }
                        cx.background_executor().timer(PROGRESS_EVERY).await;
                    }
                })
                .detach();
                cx.notify();
            });
            let prepared = {
                let version = version.clone();
                rt.spawn(async move { update::prepare(&feed, &version, &app, &running, Some(&progress)).await }).await
            };
            let _ = this.update(cx, |this, cx| match prepared.map_err(|e| Failure::Retry(e.to_string())).and_then(|r| r) {
                Ok(staged) => {
                    tracing::info!(version, staged = %staged.display(), "update: ready");
                    let status = Status::success(ready(&version));
                    this.done(Some(State::Ready { version, staged }), status, cx);
                }
                Err(Failure::Unwritable) => {
                    tracing::warn!(version, "update: the app's folder is not writable");
                    this.done(Some(State::Blocked { version, reason: UNWRITABLE }), Status::warning(UNWRITABLE), cx);
                }
                Err(Failure::Retry(error)) => {
                    tracing::warn!(version, "update failed (retrying at the next check): {error}");
                    this.done(Some(State::Idle), Status::error(format!("Couldn't download Silo {version}")), cx);
                }
                Err(Failure::Invalid(error)) => {
                    tracing::warn!(version, "update failed verification (retrying at the next check): {error}");
                    this.done(Some(State::Idle), Status::error(format!("Silo {version} failed verification")), cx);
                }
            });
        }));
    }

    fn done(&mut self, state: Option<State>, status: Status, cx: &mut Context<Self>) {
        self.check = None;
        if let Some(state) = state {
            self.state = state;
        }
        self.tell(status, cx);
        self.requesters.clear();
        cx.notify();
    }

    /// The download of `version` reached `phase` (ignored once it is over).
    fn progress(&mut self, version: &str, phase: Phase, cx: &mut Context<Self>) {
        if let State::Downloading { version: v, phase: p } = &mut self.state
            && v == version
            && *p != phase
        {
            *p = phase;
            cx.notify();
        }
    }

    /// Put the staged app in place, open it and quit. Only over this very
    /// version (else something else updated the app meanwhile; the stage is
    /// dropped), and only if the staged app still checks out.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let (State::Ready { version, staged }, Some(app), Some(running)) = (self.state.clone(), self.app.clone(), self.running.clone()) else {
            return;
        };
        if !may_install(update::app_version(&app).as_deref()) {
            tracing::info!(on_disk = ?update::app_version(&app), "update: the app changed on disk; dropping the staged update");
            drop_stage(&staged);
            self.state = State::Idle;
            cx.notify();
            return;
        }
        self.state = State::Installing;
        cx.notify();
        let installing = version.clone();
        let task = self.rt.spawn(async move {
            if let Err(error) = update::verify_app(&staged, &installing, &running).await {
                drop_stage(&staged);
                return Err(Installing::Invalid(error));
            }
            update::install(&staged, &app).map_err(Installing::Io)?;
            tracing::info!(version = installing, "update: installed; relaunching");
            update::relaunch(&app).map_err(Installing::Io)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|e| Installing::Io(std::io::Error::other(e))).and_then(|r| r);
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(Installing::Invalid(error)) => {
                    tracing::warn!("update: the staged app no longer checks out (retrying at the next check): {error}");
                    this.state = State::Idle;
                    cx.notify();
                }
                Err(Installing::Io(error)) => {
                    tracing::warn!("update install: {error}");
                    let reason = if error.kind() == std::io::ErrorKind::PermissionDenied { UNWRITABLE } else { "The update couldn't be installed" };
                    this.state = State::Blocked { version, reason };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// `silo update` replaced this app on disk: open the new one and quit.
    /// Nothing happens unless the app there is another version, so the
    /// notification (which any process may post) cannot restart Silo
    /// otherwise.
    pub fn relaunch_if_replaced(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.app.clone() else { return };
        let on_disk = update::app_version(&app);
        if on_disk.as_deref().is_none_or(|v| v == VERSION) || self.state == State::Installing {
            tracing::info!(?on_disk, "relaunch: not replaced; ignoring");
            return;
        }
        tracing::info!(?on_disk, "relaunch: updated on disk; relaunching");
        self.state = State::Installing;
        match update::relaunch(&app) {
            Ok(()) => cx.quit(),
            Err(error) => tracing::warn!("relaunch: {error}"),
        }
    }
}

/// Why [`Updater::restart`] did not install.
enum Installing {
    Invalid(String),
    Io(std::io::Error),
}

/// Remove a staged update (its staging directory).
fn drop_stage(staged: &Path) {
    if let Some(stage) = update::replaced(staged) {
        let _ = std::fs::remove_dir_all(stage);
    }
}

/// Whether an app signed as `running` updates: never an ad-hoc signed one
/// from GitHub (a local build: releases are Developer ID signed, so it
/// would download every one and refuse it); with the test feed, ad hoc
/// updates to ad hoc.
fn updates_allowed(running: &update::Identity, test_feed: bool) -> bool {
    test_feed || *running != update::Identity::AdHoc
}

/// Whether the staged update may go in over the app now on disk
/// (`on_disk`'s version): only over this running version, so a newer app
/// installed meanwhile is never downgraded.
fn may_install(on_disk: Option<&str>) -> bool {
    on_disk == Some(VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pill_shows_downloads_and_installs_with_a_spinner() {
        assert_eq!(pill(&State::Idle), None);
        let downloading = |phase| State::Downloading { version: "0.1.8".into(), phase };
        let started = downloading(Phase::Download { done: 0, total: None });
        assert_eq!(pill(&started), Some(("Downloading 0.1.8".into(), "Downloading Silo 0.1.8".into(), PillKind::Busy)));
        let sized = downloading(Phase::Download { done: 84_000_000, total: Some(200_000_000) });
        assert_eq!(pill(&sized), Some(("Downloading 0.1.8".into(), "Downloading Silo 0.1.8 (84 of 200 MB)".into(), PillKind::Progress(42))));
        let unknown = downloading(Phase::Download { done: 84_000_000, total: None });
        assert_eq!(pill(&unknown), Some(("Downloading 0.1.8 · 84 MB".into(), "Downloading Silo 0.1.8 (84 MB)".into(), PillKind::Busy)));
        let verifying = downloading(Phase::Verify);
        assert_eq!(pill(&verifying), Some(("Verifying 0.1.8…".into(), "Verifying Silo 0.1.8".into(), PillKind::Busy)));
        assert_eq!(pill(&State::Installing), Some(("Updating…".into(), "Installing and restarting".into(), PillKind::Busy)));
        let ready = State::Ready { version: "0.1.8".into(), staged: PathBuf::from("/tmp/x") };
        assert_eq!(pill(&ready), Some(("Update".into(), super::ready("0.1.8"), PillKind::Ready)));
        let blocked = State::Blocked { version: "0.1.8".into(), reason: UNWRITABLE };
        assert_eq!(pill(&blocked), Some(("Can't Update".into(), format!("Silo 0.1.8 is available. {UNWRITABLE}."), PillKind::Blocked)));
    }

    #[test]
    fn local_builds_never_update_from_github() {
        let team = update::Identity::Team { team: "FRUHQC68JC".into(), authority: "Developer ID Application: X (FRUHQC68JC)".into() };
        assert!(updates_allowed(&team, false));
        assert!(!updates_allowed(&update::Identity::AdHoc, false), "ad hoc from GitHub: no checks, no pill");
        assert!(updates_allowed(&update::Identity::AdHoc, true), "the test feed");
    }

    #[test]
    fn installs_only_over_the_running_version() {
        assert!(may_install(Some(VERSION)));
        assert!(!may_install(Some("999.0.0")), "a newer app went in meanwhile: no downgrade");
        assert!(!may_install(Some("0.0.1")));
        assert!(!may_install(None), "the app is gone");
    }

    #[test]
    fn a_dropped_stage_goes_with_its_folder() {
        let dir = std::env::temp_dir().join(format!("silo-updater-stage-{}", std::process::id()));
        let staged = dir.join(format!(".silo-update.{}", std::process::id())).join("Silo.app");
        std::fs::create_dir_all(&staged).unwrap();
        drop_stage(&staged);
        assert!(!staged.parent().unwrap().exists() && dir.exists());
        drop_stage(&dir.join("Silo.app"));
        assert!(dir.exists(), "never anything but a staging folder");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn automatic_checks_are_spaced() {
        let now = SystemTime::now();
        assert!(due(None, now), "at launch");
        assert!(!due(Some(now - Duration::from_secs(60)), now));
        assert!(due(Some(now - INTERVAL), now), "after sleeping through it too");
        assert!(due(Some(now + Duration::from_secs(3600)), now), "the clock went back");
    }
}
