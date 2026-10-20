//! Linking machines: `silo join` offers this machine's server to another
//! one, `silo up` over SSH from a Silo terminal offers it inline (OSC 7717),
//! and `silo remote install` sets up a machine over SSH and adds it.

use std::{path::Path, time::Duration};

use anyhow::{Context, bail};
use silo_client::{Control, Events, Unauthorized, transport::PairTicket};
use silo_proto::control::{Event, HostOffer, InvitePurpose, Method, OfferSource, PairInvite};

use crate::{NetOptions, bind, explain};

/// How long the device invite sent by `silo join` stays valid.
pub const JOIN_OFFER_TTL: Duration = Duration::from_secs(600);
/// How long the device invite printed over SSH stays valid.
pub const SSH_OFFER_TTL: Duration = Duration::from_secs(120);
/// How long `silo remote install`'s join invite stays valid.
pub const INSTALL_JOIN_TTL: Duration = Duration::from_secs(1800);

/// `pair.create` of `purpose`.
///
/// # Errors
/// RPC failure; [`silo_client::Offline`] is explained.
pub async fn invite(control: &Control, ttl: Duration, purpose: InvitePurpose) -> anyhow::Result<PairInvite> {
    control.call(Method::PairCreate { ttl_secs: ttl.as_secs(), purpose }).await.map_err(explain)
}

/// Fail when `control`'s server cannot be reached remotely.
///
/// # Errors
/// Remote access is unavailable, or RPC failure.
pub async fn require_remote(control: &Control) -> anyhow::Result<()> {
    match crate::server_info(control).await?.remote_error {
        Some(error) => bail!("remote access is unavailable on this server: {error}"),
        None => Ok(()),
    }
}

/// Redeem the join ticket `join` and offer the server reachable with
/// `offer_ticket` under `name`.
///
/// # Errors
/// Invalid, expired or used join ticket; the other machine is unreachable.
pub async fn offer(home: &Path, join: &str, offer_ticket: &str, name: &str, options: &NetOptions) -> anyhow::Result<()> {
    let join = PairTicket::decode(join.trim()).context("invalid join ticket")?;
    let net = bind(home, options).await?;
    let result = silo_client::offer_host(&net, &join, offer_ticket, name).await;
    let _ = net.close().await;
    result.map_err(|error| {
        if error.downcast_ref::<Unauthorized>().is_some() {
            anyhow::anyhow!("the join ticket is invalid, expired or already used; create a new one")
        } else {
            explain(error).context("could not reach the other machine")
        }
    })
}

/// Offer `local`'s server over the join ticket `join` under `name`.
///
/// # Errors
/// See [`require_remote`], [`invite`] and [`offer`].
pub async fn join(home: &Path, local: &Control, join: &str, name: &str, options: &NetOptions) -> anyhow::Result<()> {
    require_remote(local).await?;
    let invite = invite(local, JOIN_OFFER_TTL, InvitePurpose::Device).await?;
    offer(home, join, &invite.ticket, name, options).await
}

/// The offer arriving over the join invite `invite_id` (events must be
/// subscribed).
///
/// # Errors
/// None within `timeout`, or the server went away.
pub async fn wait_offer(events: &mut Events, invite_id: &str, timeout: Duration) -> anyhow::Result<HostOffer> {
    let wait = async {
        while let Some((_, event)) = events.recv().await {
            if let Event::HostOffered { offer } = event
                && matches!(&offer.source, OfferSource::Join { invite_id: id } if id == invite_id)
            {
                return Ok(offer);
            }
        }
        bail!("the local server went away")
    };
    tokio::time::timeout(timeout, wait).await.unwrap_or_else(|_| bail!("the other machine did not offer its server in time"))
}

/// Whether `silo up` should offer this machine inline: run from a Silo
/// terminal (`LC_TERMINAL=silo`, forwarded by ssh) over SSH, on a TTY.
#[must_use]
pub fn ssh_offer_wanted(lc_terminal: Option<&str>, ssh_connection: Option<&str>, tty: bool, no_offer: bool) -> bool {
    !no_offer && tty && lc_terminal == Some("silo") && ssh_connection.is_some_and(|s| !s.is_empty())
}

/// A release platform, as in the `silo-<os>-<arch>.tar.gz` assets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    LinuxX86_64,
    LinuxAarch64,
    MacosAarch64,
}

impl Platform {
    /// From `uname -sm` output.
    ///
    /// # Errors
    /// An unsupported OS or architecture.
    pub fn from_uname(uname: &str) -> anyhow::Result<Self> {
        let mut words = uname.split_whitespace();
        let (Some(os), Some(arch)) = (words.next(), words.next()) else {
            bail!("unexpected `uname -sm` output: {uname:?}");
        };
        match (os, arch) {
            ("Linux", "x86_64" | "amd64") => Ok(Self::LinuxX86_64),
            ("Linux", "aarch64" | "arm64") => Ok(Self::LinuxAarch64),
            ("Darwin", "arm64" | "aarch64") => Ok(Self::MacosAarch64),
            ("Linux" | "Darwin", arch) => bail!("unsupported architecture: {os} {arch}"),
            (os, _) => bail!("unsupported OS: {os}"),
        }
    }

    /// From an executable's header (ELF or 64-bit Mach-O).
    #[must_use]
    pub fn of_binary(header: &[u8]) -> Option<Self> {
        if header.starts_with(b"\x7fELF") {
            return match u16::from_le_bytes([*header.get(18)?, *header.get(19)?]) {
                0x3e => Some(Self::LinuxX86_64),
                0xb7 => Some(Self::LinuxAarch64),
                _ => None,
            };
        }
        if header.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
            let cpu = u32::from_le_bytes(header.get(4..8)?.try_into().ok()?);
            return (cpu == 0x0100_000c).then_some(Self::MacosAarch64);
        }
        None
    }

    /// From the executable at `path` (`None` when unrecognized).
    ///
    /// # Errors
    /// `path` cannot be read.
    pub fn of_file(path: &Path) -> anyhow::Result<Option<Self>> {
        let mut header = [0u8; 64];
        let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let n = std::io::Read::read(&mut file, &mut header)?;
        Ok(Self::of_binary(&header[..n]))
    }

    /// From `<os>-<arch>` ([`silo_proto::upgrade::platform`]).
    ///
    /// # Errors
    /// An unsupported platform.
    pub fn from_name(name: &str) -> anyhow::Result<Self> {
        match name {
            "linux-x86_64" => Ok(Self::LinuxX86_64),
            "linux-aarch64" => Ok(Self::LinuxAarch64),
            "macos-aarch64" => Ok(Self::MacosAarch64),
            other => bail!("unsupported platform: {other}"),
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::MacosAarch64 => "macos-aarch64",
        }
    }

    #[must_use]
    pub fn asset(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "silo-linux-x86_64.tar.gz",
            Self::LinuxAarch64 => "silo-linux-aarch64.tar.gz",
            Self::MacosAarch64 => "silo-macos-aarch64.tar.gz",
        }
    }
}

/// Where an uploaded binary is staged, relative to the remote home.
pub const UPLOAD_TEMP: &str = ".local/bin/.silo.upload";
/// Where `install.sh` puts a release fetched for an upgrade over ssh.
const DOWNLOAD_TEMP: &str = ".local/bin/.silo.download";

/// Why the uploaded binary's `--version` ([`Install::staged_version`])
/// failed, from its exit `status`, stdout and stderr.
#[must_use]
pub fn staged_failure(status: Option<i32>, stdout: &str, stderr: &str) -> String {
    if let Some(signal) = stdout.lines().find_map(|l| l.trim().strip_prefix("signal ")) {
        return match signal.trim_start_matches("SIG") {
            "ILL" => "Illegal instruction (built for a newer CPU?)".into(),
            "SEGV" => "Segmentation fault".into(),
            "BUS" => "Bus error".into(),
            name => format!("killed by SIG{name}"),
        };
    }
    let last = stderr.lines().map(str::trim).rfind(|l| !l.is_empty());
    match (last, status) {
        (Some(line), _) => line.to_owned(),
        (None, Some(255)) => "ssh failed".into(),
        (None, Some(code)) => format!("exit status {code}"),
        (None, None) => "killed by a signal".into(),
    }
}

/// `sh` single-quoting.
#[must_use]
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The commands `silo remote install` runs, as argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub target: String,
}

impl Install {
    /// `script` run on the target by `sh`, whatever the login shell (ssh
    /// hands its command to that shell, e.g. fish or csh); `tty` for an
    /// interactive one.
    fn ssh(&self, tty: bool, script: &str) -> Vec<String> {
        let mut argv = vec!["ssh".to_owned()];
        if tty {
            argv.push("-t".into());
        }
        argv.extend([self.target.clone(), format!("sh -c {}", quote(script))]);
        argv
    }

    /// # Errors
    /// `target` is empty or looks like an option.
    pub fn new(target: &str) -> anyhow::Result<Self> {
        if target.is_empty() || target.starts_with('-') {
            bail!("invalid ssh target {target:?}");
        }
        Ok(Self { target: target.to_owned() })
    }

    /// Detect the platform (and prepare `~/.local/bin` for an upload).
    #[must_use]
    pub fn detect(&self) -> Vec<String> {
        self.ssh(false, "mkdir -p ~/.local/bin && uname -sm")
    }

    #[must_use]
    pub fn upload(&self, binary: &Path) -> Vec<String> {
        vec!["scp".into(), "-q".into(), binary.to_string_lossy().into_owned(), format!("{}:{UPLOAD_TEMP}", self.target)]
    }

    /// Install the uploaded binary and join with `ticket`.
    #[must_use]
    pub fn join_uploaded(&self, ticket: &str) -> Vec<String> {
        let script = format!("chmod 755 ~/{UPLOAD_TEMP} && mv -f ~/{UPLOAD_TEMP} ~/.local/bin/silo && ~/.local/bin/silo join {}", quote(ticket));
        self.ssh(true, &script)
    }

    /// Detect the platform and print the installed silo's `--version` (if
    /// any) on a second line. Changes nothing there.
    #[must_use]
    pub fn probe(&self) -> Vec<String> {
        let script = "uname -sm && { ~/.local/bin/silo --version 2>/dev/null || true; }";
        self.ssh(false, script)
    }

    /// The uploaded binary's `--version`. Run by the shell, not exec'd: a
    /// signal death is printed as `signal <NAME>` and exits 128+n (ssh would
    /// otherwise just fail with 255).
    #[must_use]
    pub fn staged_version(&self) -> Vec<String> {
        let script = format!(
            "chmod 755 ~/{UPLOAD_TEMP} && {{ ~/{UPLOAD_TEMP} --version; s=$?; \
             [ $s -le 128 ] || echo \"signal $(kill -l $((s - 128)) 2>/dev/null || echo $((s - 128)))\"; exit $s; }}"
        );
        self.ssh(false, &script)
    }

    #[must_use]
    pub fn discard(&self) -> Vec<String> {
        self.ssh(false, &format!("rm -f ~/{UPLOAD_TEMP}"))
    }

    /// Install the uploaded binary and run its `silo up`, which takes over
    /// from the running server live.
    #[must_use]
    pub fn upgrade_uploaded(&self) -> Vec<String> {
        let script = format!("chmod 755 ~/{UPLOAD_TEMP} && mv -f ~/{UPLOAD_TEMP} ~/.local/bin/silo && ~/.local/bin/silo up --no-offer");
        self.ssh(false, &script)
    }

    /// Download release `version` with `install.sh`, start it and join with
    /// `ticket`. `LC_TERMINAL` is cleared so its `silo up` does not also
    /// offer inline.
    #[must_use]
    pub fn join_download(&self, version: &str, ticket: &str) -> Vec<String> {
        let script = format!(
            "curl -fsSL {} | env LC_TERMINAL= sh -s -- --version {} join {}",
            silo_proto::INSTALL_URL,
            quote(&format!("v{version}")),
            quote(ticket)
        );
        self.ssh(true, &script)
    }

    /// Prints nothing when `url` exists, `missing` when the server has no
    /// such file, else `unreachable <status>`.
    #[must_use]
    pub fn release_check(&self, url: &str) -> Vec<String> {
        let url = quote(url);
        let script = format!(
            "if command -v curl >/dev/null 2>&1; then curl -fsL -r 0-0 -o /dev/null {url}; else wget -q --spider {url}; fi; \
             s=$?; case $s in 0) ;; 22|8) echo missing ;; *) echo \"unreachable $s\" ;; esac"
        );
        self.ssh(false, &script)
    }

    /// Download release `version` with `install.sh` (which checks it) to
    /// where [`Install::upload`] puts a binary, without running it.
    #[must_use]
    pub fn stage_release(&self, version: &str) -> Vec<String> {
        let script = format!(
            "d=\"$HOME/{DOWNLOAD_TEMP}\"; rm -rf \"$d\"; mkdir -p ~/.local/bin && \
             curl -fsSL {} | SILO_INSTALL_DIR=\"$d\" sh -s -- --version {} --no-start && mv -f \"$d/silo\" ~/{UPLOAD_TEMP}; \
             s=$?; rm -rf \"$d\"; exit $s",
            silo_proto::INSTALL_URL,
            quote(&format!("v{version}"))
        );
        self.ssh(false, &script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_offer_only_from_silo_over_ssh_on_a_tty() {
        let ssh = Some("10.0.0.2 5000 10.0.0.3 22");
        assert!(ssh_offer_wanted(Some("silo"), ssh, true, false));
        assert!(!ssh_offer_wanted(Some("silo"), ssh, true, true));
        assert!(!ssh_offer_wanted(Some("silo"), ssh, false, false));
        assert!(!ssh_offer_wanted(Some("iTerm2"), ssh, true, false));
        assert!(!ssh_offer_wanted(Some(""), ssh, true, false));
        assert!(!ssh_offer_wanted(Some("silo"), None, true, false));
        assert!(!ssh_offer_wanted(Some("silo"), Some(""), true, false));
    }

    #[test]
    fn platforms_from_uname() {
        assert_eq!(Platform::from_uname("Linux x86_64\n").unwrap(), Platform::LinuxX86_64);
        assert_eq!(Platform::from_uname("Linux aarch64").unwrap(), Platform::LinuxAarch64);
        assert_eq!(Platform::from_uname("Darwin arm64").unwrap(), Platform::MacosAarch64);
        assert_eq!(Platform::from_uname("Linux arm64").unwrap().asset(), "silo-linux-aarch64.tar.gz");
        assert_eq!(Platform::LinuxX86_64.asset(), "silo-linux-x86_64.tar.gz");
        assert_eq!(Platform::MacosAarch64.asset(), "silo-macos-aarch64.tar.gz");
        assert!(Platform::from_uname("Darwin x86_64").unwrap_err().to_string().contains("unsupported architecture"));
        assert!(Platform::from_uname("FreeBSD amd64").unwrap_err().to_string().contains("unsupported OS: FreeBSD"));
        assert!(Platform::from_uname("").is_err());
    }

    #[test]
    fn platforms_of_binaries() {
        let mut elf = b"\x7fELF".to_vec();
        elf.resize(20, 0);
        elf[18] = 0x3e;
        assert_eq!(Platform::of_binary(&elf), Some(Platform::LinuxX86_64));
        elf[18] = 0xb7;
        assert_eq!(Platform::of_binary(&elf), Some(Platform::LinuxAarch64));
        elf[18] = 0x28;
        assert_eq!(Platform::of_binary(&elf), None);
        assert_eq!(Platform::of_binary(&[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 0x01]), Some(Platform::MacosAarch64));
        assert_eq!(Platform::of_binary(&[0xcf, 0xfa, 0xed, 0xfe, 0x07, 0, 0, 0x01]), None);
        assert_eq!(Platform::of_binary(b"#!/bin/sh"), None);
        assert_eq!(Platform::of_binary(b"\x7fELF"), None);
        let ours = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Some(Platform::MacosAarch64),
            ("linux", "x86_64") => Some(Platform::LinuxX86_64),
            ("linux", "aarch64") => Some(Platform::LinuxAarch64),
            _ => None,
        };
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(Platform::of_binary(&exe[..64]), ours);
    }

    #[test]
    fn platform_names() {
        for platform in [Platform::LinuxX86_64, Platform::LinuxAarch64, Platform::MacosAarch64] {
            assert_eq!(Platform::from_name(platform.name()).unwrap(), platform);
            assert_eq!(platform.asset(), format!("silo-{}.tar.gz", platform.name()));
        }
        assert!(Platform::from_name("windows-x86_64").is_err());
        let ours = Platform::of_file(&std::env::current_exe().unwrap()).unwrap();
        assert_eq!(ours.map(Platform::name), Platform::from_name(&silo_proto::upgrade::platform()).ok().map(Platform::name));
    }

    /// The staged `--version` script, run by a local `sh` as ssh would run
    /// it remotely, against stand-ins for the uploaded binary.
    #[test]
    fn staged_version_reports_why_it_failed() {
        let home = std::env::temp_dir().join(format!("silo-staged-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        let script = Install::new("me@box").unwrap().staged_version().pop().unwrap();
        let run = |body: &str| {
            std::fs::write(home.join(UPLOAD_TEMP), format!("#!/bin/sh\n{body}\n")).unwrap();
            let out = std::process::Command::new("sh").arg("-c").arg(&script).env("HOME", &home).output().unwrap();
            let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
            (out.status.code(), stdout, stderr)
        };
        let (status, stdout, _) = run("echo 'silo 0.1.0 (abc)'");
        assert_eq!((status, stdout.as_str()), (Some(0), "silo 0.1.0 (abc)\n"));
        let (status, stdout, stderr) = run("kill -ILL $$");
        assert_eq!(status, Some(128 + 4));
        assert_eq!(staged_failure(status, &stdout, &stderr), "Illegal instruction (built for a newer CPU?)");
        let (status, stdout, stderr) = run("echo \"version 'GLIBC_2.38' not found\" >&2; exit 1");
        assert_eq!(staged_failure(status, &stdout, &stderr), "version 'GLIBC_2.38' not found");
        std::fs::remove_dir_all(home).unwrap();

        assert_eq!(staged_failure(Some(139), "signal SEGV\n", ""), "Segmentation fault");
        assert_eq!(staged_failure(Some(137), "signal KILL\n", ""), "killed by SIGKILL");
        assert_eq!(staged_failure(Some(255), "", ""), "ssh failed");
        assert_eq!(staged_failure(Some(3), "", "\n"), "exit status 3");
    }

    /// The remote command ssh hands to the login shell.
    fn sh(script: &str) -> String {
        format!("sh -c {}", quote(script))
    }

    #[test]
    fn scripts_run_under_sh_whatever_the_login_shell() {
        let install = Install::new("me@box").unwrap();
        assert_eq!(install.discard(), ["ssh", "me@box", "sh -c 'rm -f ~/.local/bin/.silo.upload'"]);
        assert_eq!(install.join_uploaded("silo1abc")[..3], ["ssh", "-t", "me@box"]);
        // Single quotes inside the script survive the outer quoting.
        assert!(install.join_uploaded("silo1abc")[3].ends_with(r"silo join '\''silo1abc'\'''"));
        // The login shell may be fish or csh: the remote command is only `sh -c '…'`.
        for argv in [
            install.detect(),
            install.probe(),
            install.staged_version(),
            install.discard(),
            install.upgrade_uploaded(),
            install.stage_release("0.1.0"),
        ] {
            assert!(argv.last().unwrap().starts_with("sh -c '"), "{argv:?}");
        }
        if std::path::Path::new("/bin/csh").exists() {
            // csh would reject `{ …; }` and `$?` arithmetic run directly.
            let out = std::process::Command::new("/bin/csh").arg("-fc").arg(sh("x=$((1 + 1)); { echo \"$x\"; }")).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "2\n");
        }
    }

    #[test]
    fn upgrade_commands() {
        let install = Install::new("me@box").unwrap();
        assert_eq!(install.probe()[2], sh("uname -sm && { ~/.local/bin/silo --version 2>/dev/null || true; }"));
        assert_eq!(
            install.staged_version(),
            [
                "ssh".to_owned(),
                "me@box".to_owned(),
                sh("chmod 755 ~/.local/bin/.silo.upload && { ~/.local/bin/.silo.upload --version; s=$?; [ $s -le 128 ] || echo \"signal $(kill -l \
                    $((s - 128)) 2>/dev/null || echo $((s - 128)))\"; exit $s; }")
            ]
        );
        assert_eq!(install.discard(), ["ssh".to_owned(), "me@box".to_owned(), sh("rm -f ~/.local/bin/.silo.upload")]);
        assert_eq!(
            install.upgrade_uploaded(),
            [
                "ssh".to_owned(),
                "me@box".to_owned(),
                sh("chmod 755 ~/.local/bin/.silo.upload && mv -f ~/.local/bin/.silo.upload ~/.local/bin/silo && ~/.local/bin/silo up --no-offer")
            ]
        );
    }

    #[test]
    fn install_commands() {
        assert!(Install::new("-oProxyCommand=x").is_err());
        assert!(Install::new("").is_err());
        let install = Install::new("me@box").unwrap();
        assert_eq!(install.detect(), ["ssh".to_owned(), "me@box".to_owned(), sh("mkdir -p ~/.local/bin && uname -sm")]);
        assert_eq!(install.upload(Path::new("/tmp/my silo")), ["scp", "-q", "/tmp/my silo", "me@box:.local/bin/.silo.upload"]);
        assert_eq!(
            install.join_uploaded("silo1abc"),
            [
                "ssh".to_owned(),
                "-t".to_owned(),
                "me@box".to_owned(),
                sh("chmod 755 ~/.local/bin/.silo.upload && mv -f ~/.local/bin/.silo.upload ~/.local/bin/silo && ~/.local/bin/silo join 'silo1abc'")
            ]
        );
        assert_eq!(
            install.join_download("0.1.0", "silo1abc"),
            [
                "ssh".to_owned(),
                "-t".to_owned(),
                "me@box".to_owned(),
                sh("curl -fsSL https://nitishxyz.github.io/silo/install.sh | env LC_TERMINAL= sh -s -- --version 'v0.1.0' join 'silo1abc'")
            ]
        );
        assert_eq!(quote("a'b"), r"'a'\''b'");
    }

    /// The release scripts, run by a local `sh` as ssh would run them
    /// remotely, with `curl` replaced by a stub.
    #[test]
    fn release_scripts() {
        let home = std::env::temp_dir().join(format!("silo-release-scripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("stub")).unwrap();
        let install = Install::new("me@box").unwrap();
        let sh = |script: &str, curl: &str| {
            let stub = home.join("stub/curl");
            std::fs::write(&stub, format!("#!/bin/sh\n{curl}\n")).unwrap();
            std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            let path = format!("{}:/usr/bin:/bin", home.join("stub").display());
            let out = std::process::Command::new("sh").arg("-c").arg(script).env("HOME", &home).env("PATH", path).output().unwrap();
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };

        let check = install.release_check("https://x/v0.1.0/silo-linux-x86_64.tar.gz").pop().unwrap();
        assert!(check.contains(r"curl -fsL -r 0-0 -o /dev/null '\''https://x/v0.1.0/silo-linux-x86_64.tar.gz'\''"), "{check}");
        assert_eq!(sh(&check, "exit 0").1, "");
        assert_eq!(sh(&check, "exit 22").1, "missing\n");
        assert_eq!(sh(&check, "exit 6").1, "unreachable 6\n");

        let stage = install.stage_release("0.1.0").pop().unwrap();
        // A stand-in install.sh: records its arguments and installs a file.
        let fake = r#"echo 'echo "$*" > "$HOME/args"; mkdir -p "$SILO_INSTALL_DIR"; echo bin > "$SILO_INSTALL_DIR/silo"'"#;
        let (status, _, _) = sh(&stage, fake);
        assert_eq!(status, Some(0));
        assert_eq!(std::fs::read_to_string(home.join("args")).unwrap(), "--version v0.1.0 --no-start\n");
        assert_eq!(std::fs::read_to_string(home.join(UPLOAD_TEMP)).unwrap(), "bin\n");
        assert!(!home.join(DOWNLOAD_TEMP).exists());
        // install.sh fails: the script fails and leaves nothing behind.
        std::fs::remove_file(home.join(UPLOAD_TEMP)).unwrap();
        let (status, _, stderr) = sh(&stage, "echo 'echo \"silo: error: download failed\" >&2; exit 1'");
        assert_eq!(status, Some(1));
        assert!(stderr.contains("download failed"));
        assert!(!home.join(UPLOAD_TEMP).exists() && !home.join(DOWNLOAD_TEMP).exists());
        std::fs::remove_dir_all(home).unwrap();
    }
}
