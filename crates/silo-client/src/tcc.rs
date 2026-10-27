//! Resetting Silo Server's macOS privacy permissions (`tccutil reset`).
//!
//! System Settings lists an app's permission by bundle id, but TCC checks
//! the code requirement it stored when the entry was made. With ad-hoc
//! signing every build has a new code hash, so an older Silo Server entry
//! can stay switched on while denying the server, and System Settings
//! can't remove it. `tccutil reset <service> <bundle id>` does, for the
//! user's TCC whoever runs it; the permission is then asked for again.

use silo_proto::permissions::{PermissionId, tccutil_reset_args};

/// `tccutil` (macOS).
const TCCUTIL: &str = "/usr/bin/tccutil";

/// Reset `id` for this channel's Silo Server (blocking: a short process).
///
/// # Errors
/// `id` has no TCC service, `tccutil` failed (its message) or this is not
/// macOS.
pub fn reset(id: PermissionId) -> Result<(), String> {
    let args = tccutil_reset_args(id, silo_proto::CHANNEL.server_bundle_id()).ok_or_else(|| format!("{id} has no TCC service"))?;
    if !cfg!(target_os = "macos") {
        return Err("only macOS has privacy permissions to reset".into());
    }
    let output =
        std::process::Command::new(TCCUTIL).args(&args).stdin(std::process::Stdio::null()).output().map_err(|e| format!("run tccutil: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if message.is_empty() { format!("tccutil {} failed ({})", args.join(" "), output.status) } else { message })
}
