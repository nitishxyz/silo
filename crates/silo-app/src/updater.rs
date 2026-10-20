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

use silo_client::update::{self, Failure, Feed};

use crate::hosts_state::HostsState;

/// First automatic check after launch.
const FIRST_CHECK: Duration = Duration::from_secs(15);
/// How often whether a check is due is looked at; by the wall clock, so
/// a Mac waking from sleep checks soon after.
const TICK: Duration = Duration::from_secs(30 * 60);
/// Time between automatic checks.
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// A newer release is being downloaded and checked; nothing shows.
    Downloading(String),
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
    pub message: String,
}

pub struct Updater {
    rt: tokio::runtime::Handle,
    /// The bundle to replace; `None`: this build does not update.
    app: Option<PathBuf>,
    feed: Feed,
    state: State,
    last_check: Option<SystemTime>,
    /// The window waiting for the running check's outcome.
    requester: Option<EntityId>,
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

/// Whether an automatic check is due at `now` after one at `last`.
fn due(last: Option<SystemTime>, now: SystemTime) -> bool {
    last.is_none_or(|last| now.duration_since(last).map_or(true, |since| since >= INTERVAL))
}

impl Updater {
    pub fn install(rt: tokio::runtime::Handle, hosts: Entity<HostsState>, server_exe: &Path, cx: &mut App) {
        let (app, feed) = match Feed::from_env() {
            Some((feed, test)) => (crate::conn::updatable_app(server_exe, test), feed),
            None => (None, Feed::github()),
        };
        tracing::info!(app = ?app, "updates");
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
            Self { rt, app, feed, state: State::Idle, last_check: None, requester: None, hosts, check: None, _tick: tick }
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

    pub fn state(&self) -> &State {
        &self.state
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.hosts.read(cx).app_state().check_updates && due(self.last_check, SystemTime::now()) {
            self.check(None, cx);
        }
    }

    fn tell(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(requester) = self.requester {
            cx.emit(UpdateStatus { requester, message });
        }
    }

    /// Look for a newer release now; `requester` (a window) is told how it
    /// went.
    pub fn check(&mut self, requester: Option<EntityId>, cx: &mut Context<Self>) {
        let Some(app) = self.app.clone() else { return };
        if requester.is_some() {
            self.requester = requester;
        }
        if self.check.is_some() {
            let message = match &self.state {
                State::Downloading(version) => format!("Downloading Silo {version}…"),
                _ => "Checking for updates…".into(),
            };
            return self.tell(message, cx);
        }
        if let State::Ready { version, .. } = &self.state {
            let message = format!("{}.", ready(version));
            self.requester = None;
            if let Some(requester) = requester {
                cx.emit(UpdateStatus { requester, message });
            }
            return;
        }
        if self.state == State::Installing {
            return;
        }
        self.tell("Checking for updates…".into(), cx);
        self.last_check = Some(SystemTime::now());
        let (rt, feed) = (self.rt.clone(), self.feed.clone());
        self.check = Some(cx.spawn(async move |this, cx| {
            let latest = {
                let feed = feed.clone();
                rt.spawn(async move { update::check_latest(&feed, VERSION, &std::env::temp_dir()).await }).await
            };
            let version = match latest.map_err(|e| e.to_string()).and_then(|r| r) {
                Ok(Some(version)) => version,
                Ok(None) => return this.update(cx, |this, cx| this.done(None, "Silo is up to date.".into(), cx)).unwrap_or(()),
                Err(error) => {
                    tracing::warn!("update check: {error}");
                    return this.update(cx, |this, cx| this.done(None, "Couldn't check for updates.".into(), cx)).unwrap_or(());
                }
            };
            tracing::info!(version, "update: newer release, downloading");
            let _ = this.update(cx, |this, cx| {
                this.state = State::Downloading(version.clone());
                this.tell(format!("Downloading Silo {version}…"), cx);
            });
            let prepared = {
                let version = version.clone();
                rt.spawn(async move {
                    let running = update::signature(&app).await.map_err(|e| Failure::Retry(format!("this app: {e}")))?;
                    update::prepare(&feed, &version, &app, &running).await
                })
                .await
            };
            let _ = this.update(cx, |this, cx| match prepared.map_err(|e| Failure::Retry(e.to_string())).and_then(|r| r) {
                Ok(staged) => {
                    tracing::info!(version, staged = %staged.display(), "update: ready");
                    let message = format!("{}.", ready(&version));
                    this.done(Some(State::Ready { version, staged }), message, cx);
                }
                Err(Failure::Unwritable) => {
                    tracing::warn!(version, "update: the app's folder is not writable");
                    this.done(Some(State::Blocked { version, reason: UNWRITABLE }), format!("{UNWRITABLE}."), cx);
                }
                Err(Failure::Retry(error)) => {
                    tracing::warn!(version, "update failed (retrying at the next check): {error}");
                    this.done(Some(State::Idle), format!("Couldn't download Silo {version}."), cx);
                }
            });
        }));
    }

    fn done(&mut self, state: Option<State>, message: String, cx: &mut Context<Self>) {
        self.check = None;
        if let Some(state) = state {
            self.state = state;
        }
        self.tell(message, cx);
        self.requester = None;
        cx.notify();
    }

    /// Put the staged app in place, open it and quit.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let (State::Ready { version, staged }, Some(app)) = (self.state.clone(), self.app.clone()) else { return };
        self.state = State::Installing;
        cx.notify();
        let installing = version.clone();
        let task = self.rt.spawn_blocking(move || {
            update::install(&staged, &app)?;
            tracing::info!(version = installing, "update: installed; relaunching");
            update::relaunch(&app)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(std::io::Error::other).and_then(|r| r);
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(error) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_checks_are_spaced() {
        let now = SystemTime::now();
        assert!(due(None, now), "at launch");
        assert!(!due(Some(now - Duration::from_secs(60)), now));
        assert!(due(Some(now - INTERVAL), now), "after sleeping through it too");
        assert!(due(Some(now + Duration::from_secs(3600)), now), "the clock went back");
    }
}
