//! `silo login-server install`: the systemd unit, the empty accounts
//! file, and enabling it.

use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};

use crate::policy::Policy;

/// The unit's name.
pub const UNIT: &str = "silo-login.service";
/// Where `install` writes it.
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// `packaging/linux/silo-login.service`, with `@EXE@` for the binary
/// (kept here so the build reads nothing outside the crate's sources; a
/// test checks the two are the same).
pub const TEMPLATE: &str = "\
# systemd unit for the silo login server (docs/LOGIN_SERVER.md), written by
# `silo login-server install` with the path of the silo binary it ran as.
# The installer's copy is crates/silo-login/src/install.rs (a test keeps the
# two the same).
[Unit]
Description=silo login server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=@EXE@ login-server run
# Re-reads /etc/silo/accounts.json; devices that lost an account are revoked.
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
# The accounts' servers it starts outlive it (a restart or upgrade of the
# login server must not end anyone's sessions): stop signals only it.
KillMode=process
UMask=0077
StateDirectory=silo-login
StateDirectoryMode=0700

[Install]
WantedBy=multi-user.target
";

/// The unit for the binary at `exe`.
///
/// # Errors
/// A path systemd would split or misread.
pub fn unit(exe: &Path) -> anyhow::Result<String> {
    let exe = exe.to_str().context("the binary's path is not UTF-8")?;
    if !exe.starts_with('/') || exe.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '%' | '$' | '"' | '\'' | '\\')) {
        bail!("the binary's path {exe:?} cannot go in a unit; install silo somewhere plain (e.g. /usr/local/bin/silo)");
    }
    Ok(TEMPLATE.replace("@EXE@", exe))
}

/// What [`install`] did.
#[derive(Debug)]
pub struct Installed {
    pub unit: PathBuf,
    pub accounts: PathBuf,
    /// The accounts file was created (it existed otherwise, kept as is).
    pub created_accounts: bool,
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let dir = path.parent().context("no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// Write the unit for `exe`, create the accounts file at `accounts` (`0600`,
/// nobody allowed) unless it exists, and enable and start the service.
///
/// # Errors
/// Not root, or any step failed.
pub fn install(exe: &Path, accounts: &Path) -> anyhow::Result<Installed> {
    if !rustix::process::geteuid().is_root() {
        bail!("installing the login server needs root (sudo)");
    }
    let unit_path = Path::new(UNIT_DIR).join(UNIT);
    write_atomic(&unit_path, unit(exe)?.as_bytes(), 0o644)?;
    let created_accounts = match std::fs::symlink_metadata(accounts) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let dir = accounts.parent().context("accounts path has no directory")?;
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            Policy::empty().save(accounts)?;
            true
        }
        Err(error) => return Err(error).with_context(|| format!("stat {}", accounts.display())),
    };
    for args in [&["daemon-reload"][..], &["enable", "--now", UNIT]] {
        let status = Command::new("systemctl").args(args).status().context("run systemctl")?;
        if !status.success() {
            bail!("systemctl {} failed ({status})", args.join(" "));
        }
    }
    Ok(Installed { unit: unit_path, accounts: accounts.to_owned(), created_accounts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_this_binary_and_refuses_odd_paths() {
        let unit = unit(Path::new("/usr/local/bin/silo")).unwrap();
        assert!(unit.contains("ExecStart=/usr/local/bin/silo login-server run\n"));
        assert!(unit.contains("KillMode=process"));
        assert!(!unit.contains("@EXE@"));
        for bad in ["silo", "/opt/my silo/silo", "/x/%h/silo", "/x/$HOME/silo", "/x/a\nExecStartPre=/bin/sh"] {
            assert!(super::unit(Path::new(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_packaged_unit_is_the_installer_s() {
        // Relative to the package directory, where tests run (cargo and corgi).
        let packaged = std::fs::read_to_string("../../packaging/linux/silo-login.service").unwrap();
        assert_eq!(unit(Path::new("/usr/local/bin/silo")).unwrap(), packaged, "packaging/linux/silo-login.service and TEMPLATE differ");
    }
}
