//! `silo permissions`: the macOS privacy permissions Silo Server holds
//! (`permissions.status`), help granting one (`permissions.request`, the
//! app's permission helper), or its System Settings page.

use std::time::{Duration, Instant};

use anyhow::Context;
use silo_client::Control;
use silo_proto::{
    control::{Method, RpcError},
    permissions::{PermissionId, PermissionInfo, PermissionStatus, Permissions},
};

/// How often `request --wait` checks again.
const POLL: Duration = Duration::from_secs(1);

/// `silo permissions`: a table, or the JSON result.
pub async fn status(control: &Control, json: bool) -> anyhow::Result<i32> {
    let permissions: Permissions = control.call(Method::PermissionsStatus {}).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&permissions)?);
    } else {
        print!("{}", table(&permissions));
    }
    Ok(0)
}

/// What `silo permissions` prints.
pub fn table(permissions: &Permissions) -> String {
    let width = permissions.permissions.iter().map(|p| p.title.len()).max().unwrap_or(0);
    let mut out = String::new();
    for p in &permissions.permissions {
        out.push_str(&format!("{:<width$}  {:<11}  {}\n", p.title, label(p.status), p.reason));
    }
    match &permissions.bundle_path {
        Some(path) => out.push_str(&format!("\nGrant them to {}: {}\n", permissions.bundle_name, path.display())),
        None => out.push_str(&format!("\nGrant them to {}.\n", permissions.bundle_name)),
    }
    out.push_str("Programs already running may need restarting once granted.\n");
    out
}

fn label(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Granted => "granted",
        PermissionStatus::Denied => "not granted",
        PermissionStatus::Unknown => "unknown",
    }
}

/// `silo permissions request <id> [--wait]`: macOS's dialog (Screen
/// Recording), then the app's helper; without an app, System Settings.
pub async fn request(control: &Control, id: PermissionId, wait: bool, timeout: Duration) -> anyhow::Result<i32> {
    match control.call::<PermissionInfo>(Method::PermissionsRequest { id }).await {
        Ok(info) if info.status.granted() => {
            println!("{} is already granted to Silo Server.", info.title);
            return Ok(0);
        }
        Ok(info) => println!("Silo shows how to grant {}: drag Silo Server into the list in System Settings.", info.title),
        Err(error) if error.downcast_ref::<RpcError>().is_some_and(|e| e.code == RpcError::NO_APP) => {
            eprintln!("silo: {error}");
            open(id)?;
        }
        Err(error) => return Err(error),
    }
    if !wait {
        return Ok(0);
    }
    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL).await;
        let permissions: Permissions = control.call(Method::PermissionsStatus {}).await?;
        if permissions.get(id).is_some_and(|p| p.status.granted()) {
            println!("{} granted. Programs already running may need restarting.", id.title());
            return Ok(0);
        }
        if started.elapsed() >= timeout {
            eprintln!("silo: {} is still not granted", id.title());
            return Ok(1);
        }
    }
}

/// `silo permissions open <id>`: its page in System Settings.
pub fn open(id: PermissionId) -> anyhow::Result<i32> {
    let status = std::process::Command::new("/usr/bin/open").arg(id.settings_url()).status().context("run open")?;
    anyhow::ensure!(status.success(), "open {} failed", id.settings_url());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_lists_each_permission_and_the_bundle() {
        let permissions = Permissions {
            permissions: vec![
                PermissionId::ScreenRecording.info(PermissionStatus::Granted),
                PermissionId::FullDiskAccess.info(PermissionStatus::Denied),
                PermissionId::InputMonitoring.info(PermissionStatus::Unknown),
            ],
            bundle_path: Some("/h/silo/runtime/Silo Server.app".into()),
            bundle_name: "Silo Server".into(),
            automation_url: String::new(),
        };
        let text = table(&permissions);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Screen Recording  granted "), "{text}");
        assert!(lines[1].starts_with("Full Disk Access  not granted"), "{text}");
        assert!(lines[2].starts_with("Input Monitoring  unknown "), "{text}");
        assert!(text.contains("Grant them to Silo Server: /h/silo/runtime/Silo Server.app"), "{text}");
    }
}
