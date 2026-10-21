//! `silo remote upgrade`: replace the silo a host runs with this one (or
//! another binary) and let it take over live, over the paired connection
//! or over ssh. Both end in the host's `silo up` of the new binary.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use silo_client::{
    transport::{Conn, RemoteConn},
    upgrade::{self as stream, Refused},
};
use silo_proto::upgrade::{self, Decision, HostBuild, version_text};

use crate::{
    Endpoint, NetOptions,
    hosts::Host,
    link::{Install, Platform, staged_failure},
};

/// How long a host gets to come back on the new build. It restarts on new
/// ports, so reaching it again may take an address lookup.
const COME_BACK: Duration = Duration::from_secs(90);
/// A step taking longer than this is announced.
const SLOW: Duration = Duration::from_secs(2);

/// Where the new silo comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The published release of this silo's version, fetched by the host.
    Release,
    /// This silo, sent to the host.
    Local,
    /// This binary, sent to the host.
    Binary(PathBuf),
}

impl Source {
    /// From `--local` and `--binary` (which clap keeps apart).
    #[must_use]
    pub fn new(local: bool, binary: Option<PathBuf>) -> Self {
        match (local, binary) {
            (_, Some(path)) => Self::Binary(path),
            (true, None) => Self::Local,
            (false, None) => Self::Release,
        }
    }
}

/// What to install.
pub enum Candidate {
    /// Release `version`; its build is only known once fetched.
    Release { version: String },
    File {
        path: PathBuf,
        local: bool,
        platform: Option<Platform>,
        /// Its version and build, when it runs here.
        build: Option<(String, String)>,
    },
}

impl Candidate {
    /// # Errors
    /// Unreadable binary, or one that runs here but is no silo with a build.
    pub fn new(source: &Source) -> anyhow::Result<Self> {
        let exe = std::env::current_exe().context("locate this silo")?;
        Ok(match source {
            Source::Release => Self::Release { version: env!("CARGO_PKG_VERSION").to_owned() },
            Source::Local => Self::File {
                platform: Platform::of_file(&exe)?,
                path: exe,
                local: true,
                build: Some((env!("CARGO_PKG_VERSION").to_owned(), silo_proto::BUILD.to_owned())),
            },
            Source::Binary(path) => {
                let platform = Platform::of_file(path)?;
                let build = if platform.is_some() && platform == Platform::of_file(&exe)? { Some(version_of(path)?) } else { None };
                Self::File { path: path.clone(), local: false, platform, build }
            }
        })
    }

    /// Fail unless it installs on host `name` of `platform` (`<os>-<arch>`).
    ///
    /// # Errors
    /// No release or binary for that platform.
    pub fn check_platform(&self, name: &str, platform: &str) -> anyhow::Result<()> {
        match self {
            Self::Release { version } => match Platform::from_name(platform) {
                Ok(_) => Ok(()),
                Err(_) => bail!("{}", upgrade::not_published(version, platform)),
            },
            Self::File { path, local, platform: ours, .. } => {
                if ours.is_some_and(|ours| ours.name() == platform) {
                    Ok(())
                } else if *local {
                    bail!("this silo does not run on {name} ({platform}); drop --local to install the release")
                } else {
                    bail!("{} does not run on {name} ({platform}); pass --binary with a silo for it", path.display())
                }
            }
        }
    }

    /// The version it installs, and its build when known.
    fn known(&self) -> (&str, Option<&str>) {
        match self {
            Self::Release { version } => (version, None),
            Self::File { build: Some((version, build)), .. } => (version, Some(build)),
            Self::File { build: None, .. } => ("", None),
        }
    }
}

fn version_of(path: &Path) -> anyhow::Result<(String, String)> {
    let output = std::process::Command::new(path).arg("--version").output().with_context(|| format!("run {}", path.display()))?;
    match upgrade::parse_version(&String::from_utf8_lossy(&output.stdout)) {
        Some((version, Some(build))) => Ok((version, build)),
        _ => bail!("{} is not a silo that supports remote upgrade", path.display()),
    }
}

/// The line for a host that need not be upgraded, `None` to upgrade.
/// Without the candidate's build (a release not yet fetched) only a newer
/// running version is decided; the host decides the rest once it has it.
///
/// # Errors
/// The host runs a newer version.
pub fn settle(name: &str, running: (&str, Option<&str>), candidate: (&str, Option<&str>), force: bool) -> anyhow::Result<Option<String>> {
    let shown = running.1.map_or_else(|| running.0.to_owned(), |build| version_text(running.0, build));
    let decision = match candidate.1 {
        Some(build) => upgrade::decide(running, (candidate.0, build), force),
        None if !force && upgrade::compare_versions(running.0, candidate.0) == Some(std::cmp::Ordering::Greater) => Decision::Newer,
        None => Decision::Upgrade,
    };
    match decision {
        Decision::Upgrade => Ok(None),
        Decision::UpToDate => Ok(Some(format!("{name}: already up to date ({shown})"))),
        Decision::Newer => bail!("{name} runs {shown}, newer than {}; pass --force to downgrade", candidate.0),
    }
}
/// What happened to the sessions `before` (ids), given those `after`.
#[must_use]
pub fn kept(before: &[String], after: &[String]) -> String {
    let lost = before.iter().filter(|id| !after.contains(id)).count();
    match (before.len(), lost) {
        (0, _) => "no sessions".into(),
        (_, 0) => "sessions kept".into(),
        (n, lost) if lost == n => "sessions ended".into(),
        (n, lost) => format!("{} of {n} sessions kept", n - lost),
    }
}

/// Upgrade the stored `host` over its paired connection, saying on stderr
/// what takes long; returns the line to print.
///
/// # Errors
/// The host is unreachable or refuses, the new build could not take over,
/// or it does not come back on the new build.
pub async fn paired(home: &Path, host: &Host, candidate: &Candidate, force: bool, options: &NetOptions) -> anyhow::Result<String> {
    let opening = async {
        let own = Endpoint::own(home, options).await?;
        anyhow::Ok(match open(home, &own, host).await {
            Err(error) if error.downcast_ref::<Refused>().is_some_and(|r| r.code == "unauthorized") => {
                let device = own.refused(home, options).await?;
                let opened = open(home, &device, host).await;
                (device, opened)
            }
            opened => (own, opened),
        })
    };
    let (endpoint, opened) = slow(opening, || format!("{}: connecting…", host.name)).await?;
    let result = match opened {
        Ok(opened) => paired_on(home, &endpoint, host, opened, candidate, force).await,
        Err(error) => Err(error),
    };
    endpoint.close().await;
    result
}

async fn paired_on(
    home: &Path,
    endpoint: &Endpoint,
    host: &Host,
    (rc, opened): (RemoteConn, stream::Upgrade),
    candidate: &Candidate,
    force: bool,
) -> anyhow::Result<String> {
    let name = &host.name;
    let before = opened.host.clone();
    let running = version_text(&before.version, &before.build);
    let decided = candidate
        .check_platform(name, &before.platform)
        .and_then(|()| settle(name, (&before.version, Some(&before.build)), candidate.known(), force));
    if !matches!(decided, Ok(None)) {
        opened.close().await;
        rc.close(b"done");
        return decided.map(Option::unwrap_or_default);
    }
    let refused = |error: anyhow::Error| match error.downcast::<Refused>() {
        Ok(refused) if refused.code == upgrade::UP_TO_DATE => Ok(format!("{name}: already up to date ({running})")),
        Ok(refused) if refused.code == upgrade::NEWER => bail!("{name} {}; pass --force to downgrade", refused.reason),
        Ok(refused) => bail!("{name}: {}", refused.reason),
        Err(error) => Err(error.context(format!("upgrade {name}"))),
    };
    let sent = match candidate {
        Candidate::Release { version } => {
            eprintln!("{name}: fetching silo {version}…");
            opened.fetch_release(version, force).await
        }
        Candidate::File { path, .. } => {
            eprintln!("{name}: sending {}…", path.display());
            opened.send(path, force).await
        }
    };
    let handover = match sent {
        Ok(handover) => handover,
        Err(error) => {
            rc.close(b"done");
            return refused(error);
        }
    };
    let started = handover.started.clone();
    eprintln!("{name}: handing over to {}…", version_text(&started.version, &started.build));
    // A failed takeover is reported on the stream at once; a committed one
    // ends it, and the new build answers on a new connection.
    let failure = async {
        handover.failure().await?;
        std::future::pending().await
    };
    let result = tokio::select! {
        failed = failure => failed,
        after = come_back(home, endpoint, host, &started.build) => after,
    };
    rc.close(b"done");
    let after = match result {
        Ok(after) => after,
        Err(error) => return refused(error),
    };
    Ok(format!("{name}: {running} → {}, {}", version_text(&after.version, &after.build), kept(&before.sessions, &after.sessions)))
}

/// `future`, saying `line` on stderr once it takes longer than [`SLOW`].
async fn slow<T>(future: impl Future<Output = T>, line: impl FnOnce() -> String) -> T {
    tokio::pin!(future);
    tokio::select! {
        done = &mut future => done,
        () = tokio::time::sleep(SLOW) => {
            eprintln!("{}", line());
            future.await
        }
    }
}

/// Reach `host` as every command does ([`crate::reach`]) and open an
/// upgrade stream; the stored address is refreshed from the host's report.
async fn open(home: &Path, endpoint: &Endpoint, host: &Host) -> anyhow::Result<(RemoteConn, stream::Upgrade)> {
    let rc = crate::reach(&endpoint.net, host).await?;
    let opened = match stream::open(&Conn::Iroh(rc.clone()), endpoint.proof(host.addr.id)).await {
        Ok(opened) => opened,
        Err(error) => {
            rc.close(b"done");
            return Err(error.context(format!("reach {}", host.name)));
        }
    };
    if let Some(addr) = opened.host.endpoint_addr.clone().and_then(|addr| serde_json::from_value(addr).ok()) {
        crate::hosts::store_addr(home, addr)?;
    }
    Ok((rc, opened))
}

/// Wait for `host` to serve `build`, for at most [`COME_BACK`].
async fn come_back(home: &Path, endpoint: &Endpoint, host: &Host, build: &str) -> anyhow::Result<HostBuild> {
    let deadline = tokio::time::Instant::now() + COME_BACK;
    let attempts = async {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok((rc, opened)) = open(home, endpoint, host).await {
                let found = opened.host.clone();
                opened.close().await;
                rc.close(b"done");
                if found.build == build {
                    return found;
                }
            }
        }
    };
    match tokio::time::timeout_at(deadline, attempts).await {
        Ok(found) => Ok(found),
        Err(_) => bail!("{} did not come back on the new build within {} s; see the server log there", host.name, COME_BACK.as_secs()),
    }
}

/// Upgrade the silo at the ssh destination `target` (installed in
/// `~/.local/bin`); returns the line to print.
///
/// # Errors
/// ssh fails, no silo is installed there, or the host refuses.
pub fn over_ssh(target: &str, candidate: &Candidate, force: bool) -> anyhow::Result<String> {
    let install = Install::new(target)?;
    let probe = output(&install.probe()).with_context(|| format!("could not reach {target} over ssh"))?;
    let mut lines = probe.lines();
    let platform = Platform::from_uname(lines.next().unwrap_or_default())?;
    candidate.check_platform(target, platform.name())?;
    let Some((version, build)) = lines.next().and_then(upgrade::parse_version) else {
        bail!("no silo in ~/.local/bin on {target}; set it up with `silo remote install {target}`");
    };
    let running = build.as_deref().map_or_else(|| version.clone(), |build| version_text(&version, build));
    if let Some(line) = settle(target, (&version, build.as_deref()), candidate.known(), force)? {
        return Ok(line);
    }
    stage(&install, platform, candidate)?;
    let (new_version, new_build) = match check_staged(&install)? {
        (v, Some(b)) => (v, b),
        (_, None) => {
            run(&install.discard())?;
            bail!("the new silo predates remote upgrade (it reports no build)");
        }
    };
    match settle(target, (&version, build.as_deref()), (&new_version, Some(&new_build)), force) {
        Ok(None) => {}
        settled => {
            run(&install.discard())?;
            return settled.map(Option::unwrap_or_default);
        }
    }
    run(&install.upgrade_uploaded())?;
    Ok(format!("{target}: {running} → {}", version_text(&new_version, &new_build)))
}

/// Put `candidate` where [`Install::upload`] puts a binary on the target:
/// upload it, or have the target fetch the release.
///
/// # Errors
/// The release is not published for `platform`; ssh, scp or the download
/// fails.
pub fn stage(install: &Install, platform: Platform, candidate: &Candidate) -> anyhow::Result<()> {
    match candidate {
        Candidate::File { path, .. } => run(&install.upload(path)),
        Candidate::Release { version } => {
            check_release(install, platform, version)?;
            eprintln!("silo: downloading silo {version} on {}", install.target);
            // install.sh reports progress and PATH hints on stderr: shown only on failure.
            let argv = install.stage_release(version);
            let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().with_context(|| format!("run {}", argv[0]))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let why = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("ssh failed");
                bail!("could not download silo {version} on {}: {why}", install.target);
            }
            Ok(())
        }
    }
}

/// Fail unless release `version` has an asset for `platform`, as seen
/// from the target.
///
/// # Errors
/// Not published, or the target cannot reach the release.
pub fn check_release(install: &Install, platform: Platform, version: &str) -> anyhow::Result<()> {
    let Some(url) = upgrade::release_url(None, version, &upgrade::asset(platform.name())) else {
        bail!("{version:?} is not a release version");
    };
    match output(&install.release_check(&url))?.trim() {
        "" => Ok(()),
        "missing" => bail!("{}", upgrade::not_published(version, platform.name())),
        other => bail!("{} could not reach {url} ({other})", install.target),
    }
}

/// Version and build of the binary uploaded with [`Install::upload`], run
/// on the target: proves it runs there. Removes it when it does not.
///
/// # Errors
/// It fails to run there (e.g. built for a newer CPU, a newer glibc or
/// another platform) or is no silo; ssh fails.
pub fn check_staged(install: &Install) -> anyhow::Result<(String, Option<String>)> {
    let argv = install.staged_version();
    let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().with_context(|| format!("run {}", argv[0]))?;
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let why = if out.status.success() {
        match upgrade::parse_version(&stdout) {
            Some(version) => return Ok(version),
            None => "it is not a silo".to_owned(),
        }
    } else {
        staged_failure(out.status.code(), &stdout, &stderr)
    };
    let _ = run(&install.discard());
    bail!("the uploaded silo does not run on {}: {why}", install.target)
}

fn run(argv: &[String]) -> anyhow::Result<()> {
    let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status().with_context(|| format!("run {}", argv[0]))?;
    if !status.success() {
        bail!("{} failed ({status})", argv[0]);
    }
    Ok(())
}

fn output(argv: &[String]) -> anyhow::Result<String> {
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("run {}", argv[0]))?;
    if !output.status.success() {
        bail!("{} failed ({})", argv[0], output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles_by_version_and_build() {
        assert_eq!(settle("mini", ("0.1.0", Some("aa")), ("0.1.0", Some("bb")), false).unwrap(), None);
        assert_eq!(settle("mini", ("0.1.0", None), ("0.1.0", Some("bb")), false).unwrap(), None);
        assert_eq!(settle("mini", ("0.1.0", Some("bb")), ("0.1.0", Some("bb")), false).unwrap().unwrap(), "mini: already up to date (0.1.0 (bb))");
        let newer = settle("mini", ("0.2.0", Some("aa")), ("0.1.0", Some("bb")), false).unwrap_err().to_string();
        assert_eq!(newer, "mini runs 0.2.0 (aa), newer than 0.1.0; pass --force to downgrade");
        assert_eq!(settle("mini", ("0.2.0", Some("aa")), ("0.1.0", Some("bb")), true).unwrap(), None);
        assert_eq!(settle("mini", ("0.1.0", Some("bb")), ("0.1.0", Some("bb")), true).unwrap(), None);
        // A release's build is unknown before it is fetched: only newer versions are refused.
        assert_eq!(settle("mini", ("0.1.0", Some("bb")), ("0.1.0", None), false).unwrap(), None);
        assert_eq!(settle("mini", ("0.0.9", Some("bb")), ("0.1.0", None), false).unwrap(), None);
        assert!(settle("mini", ("0.2.0", Some("aa")), ("0.1.0", None), false).unwrap_err().to_string().contains("newer than 0.1.0"));
        assert_eq!(settle("mini", ("0.2.0", Some("aa")), ("0.1.0", None), true).unwrap(), None);
    }

    #[test]
    fn session_outcomes() {
        let ids = |ids: &[&str]| ids.iter().map(|&id| id.to_owned()).collect::<Vec<_>>();
        assert_eq!(kept(&ids(&["a", "b"]), &ids(&["b", "a"])), "sessions kept");
        assert_eq!(kept(&ids(&["a"]), &ids(&["a", "new"])), "sessions kept");
        // A fresh default session in place of the old one is not kept.
        assert_eq!(kept(&ids(&["a"]), &ids(&["fresh"])), "sessions ended");
        assert_eq!(kept(&ids(&["a", "b", "c"]), &ids(&["a", "x"])), "1 of 3 sessions kept");
        assert_eq!(kept(&[], &ids(&["x"])), "no sessions");
    }

    #[test]
    fn candidates() {
        let ours = Platform::from_name(&upgrade::platform()).unwrap();
        let other = if ours == Platform::LinuxX86_64 { Platform::LinuxAarch64 } else { Platform::LinuxX86_64 };

        let local = Candidate::new(&Source::Local).unwrap();
        assert_eq!(local.known(), (env!("CARGO_PKG_VERSION"), Some(silo_proto::BUILD)));
        local.check_platform("here", ours.name()).unwrap();
        let error = local.check_platform("box", other.name()).unwrap_err().to_string();
        assert_eq!(error, format!("this silo does not run on box ({}); drop --local to install the release", other.name()));

        // Not an executable for any platform.
        let script = std::env::temp_dir().join(format!("silo-candidate-{}", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let binary = Candidate::new(&Source::Binary(script.clone())).unwrap();
        assert_eq!(binary.known(), ("", None));
        let error = binary.check_platform("box", other.name()).unwrap_err().to_string();
        assert_eq!(error, format!("{} does not run on box ({}); pass --binary with a silo for it", script.display(), other.name()));
        std::fs::remove_file(script).unwrap();

        let release = Candidate::new(&Source::Release).unwrap();
        assert_eq!(release.known(), (env!("CARGO_PKG_VERSION"), None));
        for platform in ["linux-x86_64", "linux-aarch64", "macos-aarch64"] {
            release.check_platform("box", platform).unwrap();
        }
        let error = release.check_platform("box", "linux-riscv64").unwrap_err().to_string();
        assert_eq!(error, format!("silo {} isn't published for linux-riscv64; use --local or --binary", env!("CARGO_PKG_VERSION")));
    }
}
