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
        vec!["ssh".into(), self.target.clone(), "mkdir -p ~/.local/bin && uname -sm".into()]
    }

    #[must_use]
    pub fn upload(&self, binary: &Path) -> Vec<String> {
        vec!["scp".into(), "-q".into(), binary.to_string_lossy().into_owned(), format!("{}:{UPLOAD_TEMP}", self.target)]
    }

    /// Install the uploaded binary and join with `ticket`.
    #[must_use]
    pub fn join_uploaded(&self, ticket: &str) -> Vec<String> {
        let script = format!("chmod 755 ~/{UPLOAD_TEMP} && mv -f ~/{UPLOAD_TEMP} ~/.local/bin/silo && ~/.local/bin/silo join {}", quote(ticket));
        vec!["ssh".into(), "-t".into(), self.target.clone(), script]
    }

    /// Download the release with `install.sh`, start it and join with `ticket`.
    /// `LC_TERMINAL` is cleared so its `silo up` does not also offer inline.
    #[must_use]
    pub fn join_download(&self, ticket: &str) -> Vec<String> {
        let script = format!("curl -fsSL {} | env LC_TERMINAL= sh -s -- join {}", silo_proto::INSTALL_URL, quote(ticket));
        vec!["ssh".into(), "-t".into(), self.target.clone(), script]
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
    fn install_commands() {
        assert!(Install::new("-oProxyCommand=x").is_err());
        assert!(Install::new("").is_err());
        let install = Install::new("me@box").unwrap();
        assert_eq!(install.detect(), ["ssh", "me@box", "mkdir -p ~/.local/bin && uname -sm"]);
        assert_eq!(install.upload(Path::new("/tmp/my silo")), ["scp", "-q", "/tmp/my silo", "me@box:.local/bin/.silo.upload"]);
        assert_eq!(
            install.join_uploaded("silo1abc"),
            [
                "ssh",
                "-t",
                "me@box",
                "chmod 755 ~/.local/bin/.silo.upload && mv -f ~/.local/bin/.silo.upload ~/.local/bin/silo && ~/.local/bin/silo join 'silo1abc'"
            ]
        );
        assert_eq!(
            install.join_download("silo1abc"),
            ["ssh", "-t", "me@box", "curl -fsSL https://nitishxyz.github.io/silo/install.sh | env LC_TERMINAL= sh -s -- join 'silo1abc'"]
        );
        assert_eq!(quote("a'b"), r"'a'\''b'");
    }
}
