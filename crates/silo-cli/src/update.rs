//! `silo update` (or `silo upgrade`): this machine's Silo to the latest
//! release, by how it is installed (steps in `silo_client::update`):
//! a Mac's server comes with Silo.app (then the app is updated, and a
//! running app restarts into it; its launch takes the server over live) or
//! was installed on its own (then the runtime `Silo Server.app` is
//! replaced and its `silo up` takes over live); a standalone `silo` on
//! Linux replaces itself and runs `silo up`, as re-running install.sh
//! does. Development builds are refused.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use silo_client::update::{self, Feed, Origin};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Something `silo update` replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    App(PathBuf),
    /// The runtime server bundle of a server-only Mac.
    Server(PathBuf),
    Binary(PathBuf),
}

impl Target {
    fn label(&self) -> &'static str {
        match self {
            Self::App(_) => "Silo.app",
            Self::Server(_) => "Silo Server",
            Self::Binary(_) => "silo",
        }
    }
}

/// What to update for a `silo` installed as `origin`, with `found` the
/// Silo.app installed in one of `roots` (macOS): one target. With Silo.app
/// installed, the app owns the Mac's server.
///
/// # Errors
/// A development build, an app outside Applications, or a server bundle
/// that is not installed.
pub fn targets(origin: Origin, found: Option<PathBuf>, roots: &[PathBuf]) -> anyhow::Result<Vec<Target>> {
    match origin {
        Origin::Dev => bail!("this is a development build; update it from source"),
        Origin::App(app) => {
            let installed = app.parent().is_some_and(|dir| roots.iter().any(|r| r.canonicalize().unwrap_or_else(|_| r.clone()) == dir));
            if !installed {
                bail!("Silo.app is not in Applications; move it there to update it");
            }
            Ok(vec![Target::App(app)])
        }
        Origin::Runtime(bundle) => Ok(vec![found.map_or(Target::Server(bundle), Target::App)]),
        Origin::Bundle(bundle) => bail!("{} is not installed; run `silo up` from it first", bundle.display()),
        Origin::Standalone(_) if cfg!(target_os = "macos") => {
            bail!("on macOS silo runs as Silo Server.app; install it with install.sh (or Silo.app)")
        }
        Origin::Standalone(exe) => Ok(vec![Target::Binary(exe)]),
    }
}

/// The report for `installed` when `latest` is published: per target when
/// there are several.
#[must_use]
pub fn report(installed: &[(Target, String)], latest: &str, check: bool) -> Vec<String> {
    let several = installed.len() > 1;
    installed
        .iter()
        .map(|(target, version)| {
            let label = if several { format!("{}: ", target.label()) } else { String::new() };
            match (update::is_newer(version, latest), check) {
                (true, _) => format!("{label}{latest} available (installed {version})"),
                (false, true) => format!("{label}up to date ({version})"),
                (false, false) if several => format!("{} {version} is up to date", target.label()),
                (false, false) => format!("silo {version} is up to date"),
            }
        })
        .collect()
}

/// Run `silo update`; `check` only reports.
///
/// # Errors
/// Nothing to update here, the release cannot be fetched or checked, or a
/// replacement fails.
pub async fn run(check: bool) -> anyhow::Result<i32> {
    let (feed, test) = Feed::from_env().context("SILO_UPDATE_FEED must be a loopback http or file URL")?;
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).context("locate the silo executable")?;
    let home = silo_proto::paths::home();
    let state = home.canonicalize().unwrap_or_else(|_| home.clone());
    let user_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    // The test feed also lets a debug build outside a cargo target update.
    let origin = update::origin(&exe, &state, !cfg!(debug_assertions) || test);
    let roots = update::app_roots(&user_home, test);
    let found = if cfg!(target_os = "macos") { update::find_app(&roots) } else { None };
    let targets = targets(origin, found, &roots)?;
    let installed: Vec<(Target, String)> = targets
        .into_iter()
        .map(|target| {
            let version = match &target {
                Target::App(app) => update::app_version(app).context("Silo.app has no version")?,
                Target::Server(_) | Target::Binary(_) => VERSION.to_owned(),
            };
            anyhow::Ok((target, version))
        })
        .collect::<Result<_, _>>()?;
    if let Some(app) = installed.iter().find_map(|(t, _)| if let Target::App(app) = t { Some(app) } else { None }) {
        update::clean_stale(app);
    }
    let latest = update::latest(&feed).await.map_err(|e| anyhow!("can't check for updates: {e}"))?;
    let pending: Vec<&Target> = installed.iter().filter(|(_, v)| update::is_newer(v, &latest)).map(|(t, _)| t).collect();
    if check || pending.is_empty() {
        report(&installed, &latest, check).iter().for_each(|line| println!("{line}"));
        return Ok(0);
    }
    let failed = for_each_target(&pending, async |target| match target {
        Target::App(app) => update_app(&feed, &latest, app, &state).await,
        Target::Server(bundle) => update_server(&feed, &latest, &state, bundle).await,
        Target::Binary(exe) => update_binary(&feed, &latest, exe).await,
    })
    .await;
    if failed > 0 {
        return Ok(1);
    }
    if crate::hosts::Hosts::load(&home).is_ok_and(|h| !h.hosts.is_empty()) {
        println!("run `silo remote upgrade --all` to update remote hosts");
    }
    Ok(0)
}

/// Run `update` for every target, reporting each failure; how many failed.
async fn for_each_target<'a>(targets: &[&'a Target], update: impl AsyncFn(&'a Target) -> anyhow::Result<()>) -> usize {
    let mut failed = 0;
    for target in targets {
        if let Err(error) = update(target).await {
            eprintln!("silo: {error:#}");
            failed += 1;
        }
    }
    failed
}

#[cfg(target_os = "macos")]
async fn update_app(feed: &Feed, version: &str, app: &Path, home: &Path) -> anyhow::Result<()> {
    let running = update::signature(app).await.map_err(|e| anyhow!("can't update Silo.app: {e}"))?;
    let (progress, phases) = tokio::sync::watch::channel(update::Phase::Download { done: 0, total: None });
    let shown = std::io::IsTerminal::is_terminal(&std::io::stderr()).then(|| tokio::spawn(show_progress(version.to_owned(), phases)));
    if shown.is_none() {
        println!("downloading Silo {version}…");
    }
    let staged = update::prepare(feed, version, app, &running, Some(&progress)).await;
    drop(progress);
    if let Some(shown) = shown {
        let _ = shown.await;
    }
    let staged = staged.map_err(|e| anyhow!("can't update Silo.app: {e}"))?;
    let pids = update::running_apps(app);
    if let Err(error) = update::install(&staged, app) {
        let _ = update::replaced(&staged).map(std::fs::remove_dir_all);
        bail!("can't replace Silo.app: {error}");
    }
    println!("updated Silo.app to {version}");
    if pids.is_empty() {
        let _ = update::replaced(&staged).map(std::fs::remove_dir_all);
        // No app to take the server over at its launch: the new app's
        // server bundle does now, if this Mac runs one.
        if silo_client::runtime::server_app(home).is_dir() {
            let helper = silo_client::runtime::helper_exe(app, silo_proto::CHANNEL);
            let runtime = silo_client::runtime::install(home, &helper).with_context(|| format!("install {}", helper.display()))?;
            take_over(&runtime)?;
        }
        return Ok(());
    }
    // The old app is still running from the staging folder (removed at the
    // next launch); ask it to restart into the new one.
    update::post_relaunch();
    for _ in 0..100 {
        if !pids.iter().any(|&pid| update::alive(pid)) {
            println!("restarted Silo");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    println!("restart Silo to finish updating");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn update_app(_: &Feed, _: &str, _: &Path, _: &Path) -> anyhow::Result<()> {
    bail!("Silo.app updates only on macOS")
}

/// The line on stderr for `version` at `phase`.
#[must_use]
pub fn progress_line(version: &str, phase: update::Phase) -> String {
    match phase {
        update::Phase::Download { done: 0, total: None } => format!("downloading Silo {version}…"),
        update::Phase::Download { done, total } => format!("downloading Silo {version}… {}", update::downloaded(done, total)),
        update::Phase::Verify => format!("verifying Silo {version}…"),
    }
}

/// One updating line on stderr (a terminal) until `phases`' sender is
/// gone, then cleared.
#[cfg(target_os = "macos")]
async fn show_progress(version: String, mut phases: tokio::sync::watch::Receiver<update::Phase>) {
    use std::io::Write as _;
    loop {
        eprint!("\r\x1b[K{}", progress_line(&version, *phases.borrow_and_update()));
        let _ = std::io::stderr().flush();
        if phases.changed().await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    eprint!("\r\x1b[K");
}

async fn update_binary(feed: &Feed, version: &str, exe: &Path) -> anyhow::Result<()> {
    println!("downloading silo {version} for {}…", silo_proto::upgrade::platform());
    let staged = update::prepare_binary(feed, version, exe).await.map_err(|e| anyhow!("can't update silo: {e}"))?;
    // Renamed into place, so a running silo keeps its old binary.
    if let Err(error) = std::fs::rename(&staged, exe) {
        let _ = std::fs::remove_file(&staged);
        bail!("can't replace {}: {error}", exe.display());
    }
    println!("updated silo to {version}");
    take_over(exe)
}

/// Replace the runtime server bundle `bundle` of state directory `home`
/// with release `version`'s, then let it take over.
async fn update_server(feed: &Feed, version: &str, home: &Path, bundle: &Path) -> anyhow::Result<()> {
    println!("downloading Silo Server {version} for {}…", silo_proto::upgrade::platform());
    let staged = update::prepare_server(feed, version, home, bundle).await.map_err(|e| anyhow!("can't update Silo Server: {e}"))?;
    let swapped = silo_client::runtime::swap_in(home, &staged);
    // Holds the previous bundle after a swap (a running server keeps its
    // files), else the new one.
    let _ = silo_client::runtime::remove_any(&staged);
    swapped.with_context(|| format!("can't replace {}", bundle.display()))?;
    println!("updated Silo Server to {version}");
    take_over(&silo_client::runtime::server_exe(home))
}

/// Run the new `exe up`: it takes the running server over live.
fn take_over(exe: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new(exe).arg("up").status().context("run `silo up`")?;
    if !status.success() {
        bail!("`silo up` failed ({status}); run it again to upgrade the server");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_install_updates() {
        let roots = [PathBuf::from("/Applications"), PathBuf::from("/Users/bat/Applications")];
        let app = PathBuf::from("/Applications/Silo.app");
        let runtime = PathBuf::from("/Users/bat/Library/Application Support/silo/runtime/Silo Server.app");
        assert!(targets(Origin::Dev, Some(app.clone()), &roots).unwrap_err().to_string().contains("development build"));
        assert_eq!(targets(Origin::App(app.clone()), None, &roots).unwrap(), [Target::App(app.clone())]);
        let elsewhere = targets(Origin::App("/Volumes/Silo/Silo.app".into()), None, &roots);
        assert!(elsewhere.unwrap_err().to_string().contains("not in Applications"));
        // The runtime server bundle: the installed app owns it, else it updates itself.
        assert_eq!(targets(Origin::Runtime(runtime.clone()), Some(app.clone()), &roots).unwrap(), [Target::App(app.clone())]);
        assert_eq!(targets(Origin::Runtime(runtime.clone()), None, &roots).unwrap(), [Target::Server(runtime)]);
        let loose = targets(Origin::Bundle("/tmp/x/Silo Server.app".into()), Some(app), &roots);
        assert!(loose.unwrap_err().to_string().contains("not installed"));
        let exe = PathBuf::from("/home/me/.local/bin/silo");
        let standalone = targets(Origin::Standalone(exe.clone()), None, &roots);
        if cfg!(target_os = "macos") {
            assert!(standalone.unwrap_err().to_string().contains("Silo Server.app"));
        } else {
            assert_eq!(standalone.unwrap(), [Target::Binary(exe)]);
        }
    }

    #[tokio::test]
    async fn every_target_is_tried() {
        let app = Target::App("/Applications/Silo.app".into());
        let binary = Target::Binary("/b/silo".into());
        let tried = std::sync::Mutex::new(Vec::new());
        let failed = for_each_target(&[&app, &binary], async |target| {
            tried.lock().unwrap().push(target.clone());
            if matches!(target, Target::App(_)) { bail!("can't update Silo.app") } else { Ok(()) }
        })
        .await;
        assert_eq!(failed, 1);
        assert_eq!(*tried.lock().unwrap(), [app.clone(), binary.clone()], "the binary is updated after the app failed");
        assert_eq!(for_each_target(&[&app, &binary], async |_| bail!("no")).await, 2);
        assert_eq!(for_each_target(&[&binary], async |_| Ok(())).await, 0);
    }

    #[test]
    fn progress_lines() {
        use update::Phase;
        assert_eq!(progress_line("0.1.8", Phase::Download { done: 0, total: None }), "downloading Silo 0.1.8…");
        assert_eq!(progress_line("0.1.8", Phase::Download { done: 84_000_000, total: Some(200_000_000) }), "downloading Silo 0.1.8… 42%");
        assert_eq!(progress_line("0.1.8", Phase::Download { done: 84_000_000, total: None }), "downloading Silo 0.1.8… 84 MB");
        assert_eq!(progress_line("0.1.8", Phase::Verify), "verifying Silo 0.1.8…");
    }

    #[test]
    fn reports() {
        let binary = (Target::Binary("/b/silo".into()), "0.1.1".to_owned());
        let app = (Target::App("/Applications/Silo.app".into()), "0.1.2".to_owned());
        assert_eq!(report(std::slice::from_ref(&binary), "0.1.1", true), ["up to date (0.1.1)"]);
        assert_eq!(report(std::slice::from_ref(&binary), "0.1.2", true), ["0.1.2 available (installed 0.1.1)"]);
        assert_eq!(report(std::slice::from_ref(&binary), "0.1.1", false), ["silo 0.1.1 is up to date"]);
        assert_eq!(report(std::slice::from_ref(&binary), "0.1.0", false), ["silo 0.1.1 is up to date"], "never a downgrade");
        assert_eq!(
            report(&[app.clone(), binary.clone()], "0.1.2", true),
            ["Silo.app: up to date (0.1.2)", "silo: 0.1.2 available (installed 0.1.1)"]
        );
        assert_eq!(report(&[app, (binary.0, "0.1.2".into())], "0.1.2", false), ["Silo.app 0.1.2 is up to date", "silo 0.1.2 is up to date"]);
    }
}
