//! Hosts the app can reach: this Mac's daemon plus saved remote hosts
//! (`hosts.json`). Each host connects through a [`Transport`]; the unix
//! socket is implemented, iroh plugs in as another variant later.

use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::{Deserialize, Serialize};
use silo_client::{Control, Events};

/// A saved remote host (`hosts.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    pub id: String,
    pub name: String,
    /// iroh endpoint ticket, node id or `host:port`.
    pub address: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: Vec<RemoteHost>,
}

fn hosts_path(home: &Path) -> PathBuf {
    home.join("hosts.json")
}

#[must_use]
pub fn load(home: &Path) -> Vec<RemoteHost> {
    std::fs::read(hosts_path(home))
        .ok()
        .and_then(|b| serde_json::from_slice::<HostsFile>(&b).ok())
        .map(|f| f.hosts)
        .unwrap_or_default()
}

/// # Errors
/// Write failure.
pub fn save(home: &Path, hosts: &[RemoteHost]) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)?;
    let file = HostsFile { hosts: hosts.to_vec() };
    let tmp = hosts_path(home).with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(tmp, hosts_path(home))?;
    Ok(())
}

/// How a host's control connection is made.
#[derive(Clone, Debug)]
pub enum Transport {
    /// This Mac: adopt or spawn the daemon on the unix socket.
    Local { home: PathBuf, server_exe: PathBuf },
    /// A remote server over iroh (not implemented yet).
    Iroh { address: String },
}

impl Transport {
    /// Open a control connection with events subscribed.
    ///
    /// # Errors
    /// Connection failures; remote transports are not available yet.
    pub async fn connect(&self, client_name: &str) -> anyhow::Result<(Control, Events)> {
        match self {
            Self::Local { home, server_exe } => {
                let (control, events) = silo_client::connect_or_spawn(home, server_exe, client_name).await?;
                control.subscribe().await?;
                Ok((control, events))
            }
            Self::Iroh { address } => bail!("remote hosts are not supported yet ({address})"),
        }
    }

    /// Whether to keep retrying after a failure.
    #[must_use]
    pub fn retries(&self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

/// Connection state of a host, as shown in the switcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    Connected,
    Unreachable(String),
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub name: String,
    pub local: bool,
    pub transport: Transport,
}

impl HostInfo {
    #[must_use]
    pub fn local(home: PathBuf, server_exe: PathBuf) -> Self {
        Self { name: "This Mac".into(), local: true, transport: Transport::Local { home, server_exe } }
    }

    #[must_use]
    pub fn remote(host: &RemoteHost) -> Self {
        Self { name: host.name.clone(), local: false, transport: Transport::Iroh { address: host.address.clone() } }
    }
}

/// Pane layout density.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// Flush panes meeting at thin dividers.
    Compact,
    /// Inset rounded pane cards with small gaps.
    #[default]
    Comfortable,
}

fn yes() -> bool {
    true
}

/// App-side state and settings remembered across launches
/// (`$SILO_HOME/app.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub last_session: Option<String>,
    #[serde(default)]
    pub density: Density,
    #[serde(default = "yes")]
    pub show_pane_headers: bool,
    /// UI (chrome) font: an exact installed family, or empty for Automatic
    /// (the macOS system font). Separate from the terminal font in
    /// `terminal.json`. Absent in older files, which keeps the default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ui_font_family: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self { last_session: None, density: Density::default(), show_pane_headers: true, ui_font_family: String::new() }
    }
}

#[must_use]
pub fn load_state(home: &Path) -> AppState {
    std::fs::read(home.join("app.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(home: &Path, state: &AppState) {
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(home.join("app.json"), bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_round_trip() {
        let dir = std::env::temp_dir().join(format!("silo-hosts-{}", std::process::id()));
        let hosts = vec![RemoteHost { id: "h1".into(), name: "mini".into(), address: "mini.local".into() }];
        save(&dir, &hosts).unwrap();
        assert_eq!(load(&dir), hosts);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_default_and_round_trip() {
        // Older files without settings get the reference design.
        let old: AppState = serde_json::from_str(r#"{"last_session":"s1"}"#).unwrap();
        assert_eq!(old.density, Density::Comfortable);
        assert!(old.show_pane_headers);
        assert_eq!(old.ui_font_family, "", "Automatic UI font");
        let dir = std::env::temp_dir().join(format!("silo-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState { last_session: None, density: Density::Compact, show_pane_headers: false, ui_font_family: String::new() };
        save_state(&dir, &state);
        let text = std::fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(!text.contains("ui_font_family"), "Automatic is not written: {text}");
        let back = load_state(&dir);
        assert_eq!((back.density, back.show_pane_headers), (Density::Compact, false));
        // The exact family persists, independent of the terminal font.
        let state = AppState { ui_font_family: "Helvetica Neue".into(), ..state };
        save_state(&dir, &state);
        assert_eq!(load_state(&dir).ui_font_family, "Helvetica Neue");
        assert!(!dir.join("terminal.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
