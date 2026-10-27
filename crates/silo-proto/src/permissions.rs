//! macOS privacy permissions of Silo Server (`permissions.status`,
//! `permissions.request`; feature [`crate::compat::feature::PERMISSIONS`]).
//!
//! The server is the responsible process of every program in Silo's
//! terminals, so what TCC granted *it* is what those programs get. The
//! server checks in a short-lived child of its own (`silo permissions
//! probe`, which prints [`ProbeReport`] as JSON): that child inherits the
//! server's responsibility, and asking from a fresh process every time
//! dodges TCC's per-process caching. Local only: a remote or paired client
//! never reads or asks for them ([`crate::access::Need::Local`]).

use std::{collections::BTreeMap, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A privacy permission programs in Silo terminals may need.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionId {
    ScreenRecording,
    Accessibility,
    FullDiskAccess,
    InputMonitoring,
    /// A newer permission; skipped.
    #[serde(other)]
    Unknown,
}

/// Whether a permission is granted to Silo Server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    Granted,
    Denied,
    /// Not determined, or the check could not tell (also a newer status).
    #[default]
    #[serde(other)]
    Unknown,
}

/// System Settings › Privacy & Security › Automation: macOS asks for it
/// per target app the first time a program controls another app, so it is
/// only linked, never checked.
pub const AUTOMATION_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";

/// System Settings › Privacy & Security.
const PRIVACY_URL: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy";

impl PermissionId {
    /// Every permission checked, in the order they are shown.
    pub const ALL: [Self; 4] = [Self::ScreenRecording, Self::Accessibility, Self::FullDiskAccess, Self::InputMonitoring];

    /// Its name in System Settings.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Screen Recording",
            Self::Accessibility => "Accessibility",
            Self::FullDiskAccess => "Full Disk Access",
            Self::InputMonitoring => "Input Monitoring",
            Self::Unknown => "Unknown permission",
        }
    }

    /// One line on what it lets programs do.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::ScreenRecording => "Lets programs in Silo terminals capture the screen",
            Self::Accessibility => "Lets programs in Silo terminals control your Mac and other apps",
            Self::FullDiskAccess => "Lets programs in Silo terminals read protected files, like Mail, Messages and Safari data",
            Self::InputMonitoring => "Lets programs in Silo terminals read the keyboard and mouse in other apps",
            Self::Unknown => "Needed by some programs in Silo terminals",
        }
    }

    /// Its page in System Settings › Privacy & Security.
    #[must_use]
    pub const fn settings_url(self) -> &'static str {
        match self {
            Self::ScreenRecording => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            Self::Accessibility => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            Self::FullDiskAccess => "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
            Self::InputMonitoring => "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
            Self::Unknown => PRIVACY_URL,
        }
    }

    /// The wire name (`screen_recording`).
    #[must_use]
    pub fn name(self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
    }

    /// [`PermissionInfo`] of this permission with `status`.
    #[must_use]
    pub fn info(self, status: PermissionStatus) -> PermissionInfo {
        PermissionInfo { id: self, title: self.title().into(), reason: self.reason().into(), settings_url: self.settings_url().into(), status }
    }
}

impl std::fmt::Display for PermissionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

/// A permission by its wire name, `-` or `_` between words, any case
/// (`screen-recording`, `full_disk_access`).
impl std::str::FromStr for PermissionId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let wanted = s.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL.into_iter().find(|id| id.name() == wanted).ok_or_else(|| {
            let names: Vec<String> = Self::ALL.iter().map(|id| id.name().replace('_', "-")).collect();
            format!("unknown permission {s:?} (one of: {})", names.join(", "))
        })
    }
}

impl PermissionStatus {
    #[must_use]
    pub const fn granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

/// One permission, as `permissions.status` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionInfo {
    pub id: PermissionId,
    pub title: String,
    /// One line on what it lets programs do.
    pub reason: String,
    /// Its page in System Settings (`x-apple.systempreferences:…`).
    pub settings_url: String,
    pub status: PermissionStatus,
}

/// Result of `permissions.status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Permissions {
    pub permissions: Vec<PermissionInfo>,
    /// The Silo Server app the server runs from (its runtime copy): what to
    /// drag into System Settings' list. `None` for a server not running
    /// from an app bundle.
    #[serde(default)]
    pub bundle_path: Option<PathBuf>,
    /// The name System Settings shows for it (`Silo Server`).
    pub bundle_name: String,
    /// [`AUTOMATION_URL`].
    #[serde(default)]
    pub automation_url: String,
}

impl Permissions {
    /// `id`'s entry.
    #[must_use]
    pub fn get(&self, id: PermissionId) -> Option<&PermissionInfo> {
        self.permissions.iter().find(|p| p.id == id)
    }
}

/// What `silo permissions probe` prints: each permission's status as seen
/// from the probing process.
pub type ProbeReport = BTreeMap<PermissionId, PermissionStatus>;

/// A probe's output (JSON [`ProbeReport`]) as every permission's status:
/// unreported ones are unknown, unknown names are skipped.
///
/// # Errors
/// The output is not a JSON object.
pub fn parse_probe(output: &str) -> Result<ProbeReport, String> {
    let line = output.lines().rev().find(|l| l.trim_start().starts_with('{')).ok_or_else(|| format!("no probe report in {output:?}"))?;
    let mut report: ProbeReport = serde_json::from_str(line.trim()).map_err(|e| format!("probe report: {e}"))?;
    report.remove(&PermissionId::Unknown);
    for id in PermissionId::ALL {
        report.entry(id).or_default();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ids_have_snake_case_names_and_their_settings_pages() {
        let expected = [
            (PermissionId::ScreenRecording, "screen_recording", "Privacy_ScreenCapture"),
            (PermissionId::Accessibility, "accessibility", "Privacy_Accessibility"),
            (PermissionId::FullDiskAccess, "full_disk_access", "Privacy_AllFiles"),
            (PermissionId::InputMonitoring, "input_monitoring", "Privacy_ListenEvent"),
        ];
        assert_eq!(PermissionId::ALL.len(), expected.len());
        for (id, name, anchor) in expected {
            assert_eq!(serde_json::to_value(id).unwrap(), json!(name));
            assert_eq!(serde_json::from_value::<PermissionId>(json!(name)).unwrap(), id);
            assert_eq!(id.name(), name);
            assert_eq!(id.to_string(), name);
            assert_eq!(id.settings_url(), format!("x-apple.systempreferences:com.apple.preference.security?{anchor}"));
            assert_eq!(name.parse::<PermissionId>().unwrap(), id);
            assert_eq!(name.replace('_', "-").to_uppercase().parse::<PermissionId>().unwrap(), id);
            assert!(id.reason().starts_with("Lets programs in Silo terminals"), "{id}");
        }
        assert!(AUTOMATION_URL.ends_with("?Privacy_Automation"));
        assert_eq!(serde_json::from_value::<PermissionId>(json!("camera")).unwrap(), PermissionId::Unknown);
        assert!("camera".parse::<PermissionId>().unwrap_err().contains("screen-recording"));
        assert_eq!(serde_json::to_value(PermissionStatus::Denied).unwrap(), json!("denied"));
        assert_eq!(serde_json::from_value::<PermissionStatus>(json!("restricted")).unwrap(), PermissionStatus::Unknown);
    }

    #[test]
    fn probe_reports_parse_with_every_permission() {
        let report = parse_probe("noise\n{\"screen_recording\":\"granted\",\"accessibility\":\"denied\",\"camera\":\"granted\"}\n").unwrap();
        assert_eq!(report.len(), PermissionId::ALL.len());
        assert_eq!(report[&PermissionId::ScreenRecording], PermissionStatus::Granted);
        assert_eq!(report[&PermissionId::Accessibility], PermissionStatus::Denied);
        assert_eq!(report[&PermissionId::FullDiskAccess], PermissionStatus::Unknown, "unreported");
        assert_eq!(report[&PermissionId::InputMonitoring], PermissionStatus::Unknown);
        assert!(!report.contains_key(&PermissionId::Unknown));
        assert!(parse_probe("").is_err());
        assert!(parse_probe("{not json").is_err());
        // What a probe prints round-trips.
        let printed = serde_json::to_string(&report).unwrap();
        assert_eq!(parse_probe(&printed).unwrap(), report);
    }

    #[test]
    fn info_carries_the_static_texts() {
        let info = PermissionId::FullDiskAccess.info(PermissionStatus::Granted);
        assert_eq!(info.title, "Full Disk Access");
        assert!(info.settings_url.ends_with("Privacy_AllFiles"));
        let all = Permissions {
            permissions: vec![info.clone()],
            bundle_path: None,
            bundle_name: "Silo Server".into(),
            automation_url: AUTOMATION_URL.into(),
        };
        assert_eq!(all.get(PermissionId::FullDiskAccess), Some(&info));
        assert_eq!(all.get(PermissionId::Accessibility), None);
    }
}
