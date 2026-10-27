//! `permissions.status` and `permissions.request` (local only): macOS
//! privacy permissions as Silo Server holds them.
//!
//! TCC charges a program in a Silo terminal to its responsible process,
//! the server (it disclaims its own launcher at start), so the server is
//! what needs Screen Recording, Accessibility and the rest. Each check runs
//! in a short-lived child, `silo permissions probe` started from the
//! server's own executable without disclaiming: it is charged to the
//! server like every shell, and a fresh process never sees a status TCC
//! cached in this one before the user changed it. The child calls the
//! native checks ([`probe`]) and prints a [`ProbeReport`].

use std::{path::PathBuf, time::Duration};

use serde_json::Value;
use silo_proto::{
    control::{Method, RpcError},
    permissions::{AUTOMATION_URL, PermissionId, PermissionStatus, Permissions, ProbeReport, parse_probe, server_bundle},
};

use crate::Server;

/// How long a probe child may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The CLI words that run [`probe_main`] (`silo permissions probe`).
pub const PROBE_ARGS: [&str; 2] = ["permissions", "probe"];
/// `--request <id>`: ask macOS for `id` first (Screen Recording only).
pub const REQUEST_FLAG: &str = "--request";

pub async fn handle(server: &Server, client_id: u64, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::PermissionsStatus {} => to_value(status().await?),
        Method::PermissionsRequest { id } => {
            if id == PermissionId::Unknown {
                return Err(RpcError::bad_request("unknown permission"));
            }
            if id == PermissionId::ScreenRecording {
                // macOS's own dialog, once per bundle; it also lists Silo
                // Server in Screen Recording, so the user only switches it on.
                let _ = run_probe(Some(id)).await.inspect_err(|e| tracing::warn!("permissions: request {id}: {}", e.message));
            }
            let status = run_probe(None).await?.get(&id).copied().unwrap_or_default();
            if status.granted() {
                return to_value(id.info(status));
            }
            let show = Method::AppPermissionsShow { id, client_id: None };
            match crate::app_bridge::handle(server, client_id, show).await {
                Ok(_) => to_value(id.info(status)),
                Err(error) if error.code == RpcError::NO_APP => Err(RpcError::new(
                    RpcError::NO_APP,
                    format!(
                        "No Silo app is connected to help grant {}: add {} in System Settings › Privacy & Security › {}",
                        id.title(),
                        silo_proto::CHANNEL.server_name(),
                        id.title()
                    ),
                )
                .with_data(serde_json::json!({ "settings_url": id.settings_url() }))),
                Err(error) => Err(error),
            }
        }
        _ => Err(RpcError::bad_request("not a permissions method")),
    }
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

/// Every permission's status as the server holds it, with the bundle to
/// drag into System Settings.
async fn status() -> Result<Permissions, RpcError> {
    let report = run_probe(None).await?;
    let exe = std::env::current_exe().ok();
    Ok(Permissions {
        permissions: PermissionId::ALL.into_iter().map(|id| id.info(report.get(&id).copied().unwrap_or_default())).collect(),
        bundle_path: exe.as_deref().and_then(server_bundle),
        bundle_name: silo_proto::CHANNEL.server_name().into(),
        automation_url: AUTOMATION_URL.into(),
    })
}

/// The args of a probe child: [`PROBE_ARGS`], then `--request <id>`.
#[must_use]
pub fn probe_args(request: Option<PermissionId>) -> Vec<String> {
    let mut args: Vec<String> = PROBE_ARGS.iter().map(|&a| a.to_owned()).collect();
    if let Some(id) = request {
        args.extend([REQUEST_FLAG.to_owned(), id.name().replace('_', "-")]);
    }
    args
}

/// Run `silo permissions probe` from this server's own executable, as its
/// child (so as the server, for TCC), and read its report.
async fn run_probe(request: Option<PermissionId>) -> Result<ProbeReport, RpcError> {
    let exe = std::env::current_exe().map_err(|e| RpcError::internal(format!("locate the server executable: {e}")))?;
    let child = tokio::process::Command::new(&exe)
        .args(probe_args(request))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RpcError::internal(format!("start the permission probe: {e}")))?;
    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| RpcError::new(RpcError::TIMEOUT, "the permission probe did not answer"))?
        .map_err(|e| RpcError::internal(format!("permission probe: {e}")))?;
    parse_probe(&String::from_utf8_lossy(&output.stdout)).map_err(RpcError::internal)
}

/// `silo permissions probe [--request <id>]`: in the probe child, ask
/// macOS for `request` (Screen Recording only: its dialog), then print
/// every permission's status as a JSON [`ProbeReport`] line.
pub fn probe_main(request: Option<PermissionId>) {
    if request == Some(PermissionId::ScreenRecording) {
        native::request_screen_recording();
    }
    println!("{}", serde_json::to_string(&probe()).unwrap_or_else(|_| "{}".into()));
}

/// Every permission's status as this process holds it (native checks).
#[must_use]
pub fn probe() -> ProbeReport {
    PermissionId::ALL.into_iter().map(|id| (id, check(id))).collect()
}

fn check(id: PermissionId) -> PermissionStatus {
    match id {
        PermissionId::ScreenRecording => native::screen_recording(),
        PermissionId::Accessibility => native::accessibility(),
        PermissionId::InputMonitoring => native::input_monitoring(),
        PermissionId::FullDiskAccess => full_disk_access(),
        PermissionId::Unknown => PermissionStatus::Unknown,
    }
}

/// Places only Full Disk Access may read: the user's and the system's TCC
/// databases (the user's is missing on some Macs) and Safari's data.
fn protected_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        paths.push(home.join("Library/Application Support/com.apple.TCC/TCC.db"));
    }
    paths.push(PathBuf::from("/Library/Application Support/com.apple.TCC/TCC.db"));
    if let Some(home) = &home {
        paths.push(home.join("Library/Safari"));
    }
    paths
}

fn full_disk_access() -> PermissionStatus {
    if !cfg!(target_os = "macos") {
        return PermissionStatus::Unknown;
    }
    let read = |path: PathBuf| if path.is_dir() { std::fs::read_dir(path).map(drop) } else { std::fs::File::open(path).map(drop) };
    protected_paths().into_iter().map(read).map(fda_status).find(|s| *s != PermissionStatus::Unknown).unwrap_or_default()
}

/// Full Disk Access from reading a TCC-protected file: read is granted,
/// refused (`EPERM`, `EACCES`) denied, anything else (missing) unknown.
#[must_use]
pub fn fda_status(opened: std::io::Result<()>) -> PermissionStatus {
    match opened {
        Ok(()) => PermissionStatus::Granted,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => PermissionStatus::Denied,
        Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => PermissionStatus::Denied,
        Err(_) => PermissionStatus::Unknown,
    }
}

/// `IOHIDCheckAccess`'s `IOHIDAccessType`: 0 granted, 1 denied, 2 unknown.
#[must_use]
pub fn hid_access_status(access: u32) -> PermissionStatus {
    match access {
        0 => PermissionStatus::Granted,
        1 => PermissionStatus::Denied,
        _ => PermissionStatus::Unknown,
    }
}

#[cfg(target_os = "macos")]
mod native {
    use silo_proto::permissions::PermissionStatus;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        // `Boolean`: an unsigned char.
        fn AXIsProcessTrusted() -> u8;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// `IOHIDRequestType` in, `IOHIDAccessType` out.
        fn IOHIDCheckAccess(request: u32) -> u32;
    }

    /// `kIOHIDRequestTypeListenEvent`.
    const LISTEN_EVENT: u32 = 1;

    fn granted(yes: bool) -> PermissionStatus {
        if yes { PermissionStatus::Granted } else { PermissionStatus::Denied }
    }

    pub fn screen_recording() -> PermissionStatus {
        // SAFETY: a plain query.
        granted(unsafe { CGPreflightScreenCaptureAccess() })
    }

    pub fn request_screen_recording() {
        // SAFETY: shows macOS's dialog (once) and returns.
        let _ = unsafe { CGRequestScreenCaptureAccess() };
    }

    pub fn accessibility() -> PermissionStatus {
        // SAFETY: a plain query.
        granted(unsafe { AXIsProcessTrusted() } != 0)
    }

    pub fn input_monitoring() -> PermissionStatus {
        // SAFETY: a plain query; never prompts.
        super::hid_access_status(unsafe { IOHIDCheckAccess(LISTEN_EVENT) })
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use silo_proto::permissions::PermissionStatus;

    pub fn screen_recording() -> PermissionStatus {
        PermissionStatus::Unknown
    }
    pub fn request_screen_recording() {}
    pub fn accessibility() -> PermissionStatus {
        PermissionStatus::Unknown
    }
    pub fn input_monitoring() -> PermissionStatus {
        PermissionStatus::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_children_get_the_probe_words() {
        assert_eq!(probe_args(None), ["permissions", "probe"]);
        assert_eq!(probe_args(Some(PermissionId::ScreenRecording)), ["permissions", "probe", "--request", "screen-recording"]);
    }

    #[test]
    fn full_disk_access_reads_from_opening_a_protected_file() {
        use std::io::{Error, ErrorKind};
        assert_eq!(fda_status(Ok(())), PermissionStatus::Granted);
        assert_eq!(fda_status(Err(Error::from_raw_os_error(libc::EPERM))), PermissionStatus::Denied);
        assert_eq!(fda_status(Err(Error::from_raw_os_error(libc::EACCES))), PermissionStatus::Denied);
        assert_eq!(fda_status(Err(Error::from(ErrorKind::NotFound))), PermissionStatus::Unknown);
        assert_eq!(fda_status(Err(Error::other("odd"))), PermissionStatus::Unknown);
        let paths = protected_paths();
        assert!(paths.iter().all(|p| p.is_absolute()), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("Library/Application Support/com.apple.TCC/TCC.db")));
    }

    #[test]
    fn input_monitoring_maps_hid_access() {
        assert_eq!(hid_access_status(0), PermissionStatus::Granted);
        assert_eq!(hid_access_status(1), PermissionStatus::Denied);
        assert_eq!(hid_access_status(2), PermissionStatus::Unknown);
        assert_eq!(hid_access_status(7), PermissionStatus::Unknown);
    }
}
