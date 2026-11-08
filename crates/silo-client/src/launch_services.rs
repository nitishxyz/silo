//! Keeping Launch Services' record of Silo's bundles current. macOS
//! registers a bundle wherever it is checked or run (an update's staging
//! copy, a mounted image) and keeps the record after the folder is gone;
//! Notification Center, the Dock and privacy prompts then may resolve a
//! bundle id to a deleted copy and show no icon. Every swap re-registers
//! the bundle in place and unregisters the copy that is about to go.

use std::path::Path;

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// Register `bundle` again (forced: its current files, icon included).
/// Bundles in temporary folders (tests, scratch copies) are left out: they
/// would only add records of folders about to go.
pub fn refresh(bundle: &Path) {
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let scratch = ["/tmp", "/private/tmp", "/private/var/folders", "/var/folders"].iter().any(|d| bundle.starts_with(d)) || bundle.starts_with(&temp);
    if !scratch {
        run(&["-f"], bundle);
    }
}

/// Unregister `bundle`'s record (call before removing the folder).
pub fn forget(bundle: &Path) {
    run(&["-u"], bundle);
}

fn run(flags: &[&str], bundle: &Path) {
    if !cfg!(target_os = "macos") || !Path::new(LSREGISTER).exists() {
        return;
    }
    let out = std::process::Command::new(LSREGISTER)
        .args(flags)
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(error) = out {
        tracing::debug!("lsregister {flags:?} {}: {error}", bundle.display());
    }
}
