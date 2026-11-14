//! The accounts file (`/etc/silo/accounts.json`, `docs/LOGIN_SERVER.md`
//! §3): which device may use which account.
//!
//! Kept as the JSON document it was read from, so a rewrite (an invite
//! redeemed, `allow`, `deny`) keeps every key this build does not know.

use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use silo_proto::login::LoginError;
use silo_transport::{EndpointId, valid_account};

use crate::code;

/// The file's format version.
pub const VERSION: u64 = 1;
/// The account name whose devices may use any regular account.
pub const ANY: &str = "*";
/// Largest accounts file read.
const MAX_FILE: u64 = 4 * 1024 * 1024;

/// A device entry: its endpoint id and an optional label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// The canonical form of a device id (an iroh endpoint id).
///
/// # Errors
/// Not an endpoint id.
pub fn device_id(text: &str) -> anyhow::Result<String> {
    text.trim().parse::<EndpointId>().map(|id| id.to_string()).with_context(|| format!("{text:?} is not a device id"))
}

/// `"id"` or `"id name"`.
fn parse_entry(text: &str) -> anyhow::Result<Device> {
    let text = text.trim();
    let (id, name) = text.split_once(char::is_whitespace).map_or((text, ""), |(id, name)| (id, name.trim()));
    Ok(Device { id: device_id(id)?, name: name.to_owned() })
}

fn refused(code: &str, message: impl Into<String>) -> LoginError {
    LoginError { code: code.to_owned(), message: message.into() }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    doc: Map<String, Value>,
    /// Parsed view of `doc["accounts"]`.
    accounts: BTreeMap<String, Vec<Device>>,
    /// Parsed view of `doc["defaults"]`: device → account.
    defaults: BTreeMap<String, String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::empty()
    }
}

impl Policy {
    /// What `install` writes: nobody gets in.
    #[must_use]
    pub fn empty() -> Self {
        let Value::Object(doc) = json!({"version": VERSION, "accounts": {}, "defaults": {}}) else { unreachable!() };
        Self { doc, accounts: BTreeMap::new(), defaults: BTreeMap::new() }
    }

    /// Parse and check a whole file: a bad entry fails it, so a typo is
    /// found at reload instead of silently letting nobody (or somebody) in.
    ///
    /// # Errors
    /// Not JSON, an unknown version, a bad account name or device id.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let Value::Object(doc) = serde_json::from_slice(bytes).context("accounts file is not JSON")? else {
            bail!("accounts file is not a JSON object");
        };
        match doc.get("version") {
            None => {}
            Some(v) if v.as_u64() == Some(VERSION) => {}
            Some(v) => bail!("accounts file version {v} is not supported (this build reads {VERSION})"),
        }
        let mut accounts = BTreeMap::new();
        if let Some(value) = doc.get("accounts") {
            let map = value.as_object().context("\"accounts\" is not an object")?;
            for (account, entry) in map {
                if account != ANY && !valid_account(account) {
                    bail!("{account:?} is not a user name");
                }
                let devices = match entry.get("devices") {
                    None => Vec::new(),
                    Some(list) => list
                        .as_array()
                        .with_context(|| format!("\"{account}\".devices is not a list"))?
                        .iter()
                        .map(|d| d.as_str().context("a device is not a string").and_then(parse_entry))
                        .collect::<anyhow::Result<_>>()
                        .with_context(|| format!("account {account:?}"))?,
                };
                accounts.insert(account.clone(), devices);
            }
        }
        let mut defaults = BTreeMap::new();
        if let Some(value) = doc.get("defaults") {
            for (device, account) in value.as_object().context("\"defaults\" is not an object")? {
                let account = account.as_str().with_context(|| format!("default of {device} is not a string"))?;
                if !valid_account(account) {
                    bail!("default {account:?} is not a user name");
                }
                defaults.insert(device_id(device)?, account.to_owned());
            }
        }
        Ok(Self { doc, accounts, defaults })
    }

    /// Read `path` without following a symlink; a missing file is
    /// [`Policy::empty`]. As root, a file not owned by root or writable by
    /// others is refused: it decides who gets in as whom.
    ///
    /// # Errors
    /// Unreadable, unsafe or invalid file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
        };
        let meta = file.metadata().with_context(|| format!("stat {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if rustix::process::geteuid().is_root() && (meta.uid() != 0 || meta.mode() & 0o022 != 0) {
            bail!("{} must be owned by root and writable only by it", path.display());
        }
        if meta.len() > MAX_FILE {
            bail!("{} is too large", path.display());
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_FILE), &mut bytes)?;
        Self::parse(&bytes).with_context(|| format!("read {}", path.display()))
    }

    /// Write to `path` atomically, `0600`: a fresh file beside it
    /// (`O_EXCL|O_NOFOLLOW`), synced, then renamed over it. A symlink at
    /// `path` is replaced, never followed.
    ///
    /// # Errors
    /// I/O failure.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = path.file_name().context("accounts path has no file name")?.to_string_lossy();
        let tmp = dir.join(format!(".{name}.{}.{:08x}.tmp", std::process::id(), rand::random::<u32>()));
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        let written = (|| {
            out.write_all(&serde_json::to_vec_pretty(&self.doc)?)?;
            out.write_all(b"\n")?;
            out.sync_all()?;
            std::fs::rename(&tmp, path)?;
            anyhow::Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.with_context(|| format!("write {}", path.display()))?;
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// The accounts and their devices, `*` included.
    #[must_use]
    pub fn accounts(&self) -> &BTreeMap<String, Vec<Device>> {
        &self.accounts
    }

    #[must_use]
    pub fn defaults(&self) -> &BTreeMap<String, String> {
        &self.defaults
    }

    /// Every `(account, device)` the file lists, `*` included.
    #[must_use]
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.accounts.iter().flat_map(|(account, devices)| devices.iter().map(|d| (account.clone(), d.id.clone()))).collect()
    }

    fn lists(&self, account: &str, device: &str) -> bool {
        self.accounts.get(account).is_some_and(|devices| devices.iter().any(|d| d.id == device))
    }

    /// The accounts that name `device` (not `*`).
    #[must_use]
    pub fn accounts_of(&self, device: &str) -> Vec<String> {
        self.accounts.iter().filter(|(a, devices)| *a != ANY && devices.iter().any(|d| d.id == device)).map(|(a, _)| a.clone()).collect()
    }

    /// Whether `device` may use `account`: named for it, or listed under
    /// `*` and `account` is a regular one (`regular` says which are).
    pub fn allows(&self, device: &str, account: &str, regular: &dyn Fn(&str) -> bool) -> bool {
        self.lists(account, device) || (self.lists(ANY, device) && regular(account))
    }

    /// The account `device` logs in as: `asked`, else its default, else
    /// its only account.
    ///
    /// # Errors
    /// `not_allowed` when it may not use the account asked for, or any;
    /// `choose_account` naming the choices when it has several.
    pub fn choose(&self, device: &str, asked: Option<&str>, regular: &dyn Fn(&str) -> bool) -> Result<String, LoginError> {
        if let Some(asked) = asked {
            return if self.allows(device, asked, regular) {
                Ok(asked.to_owned())
            } else {
                Err(refused(code::NOT_ALLOWED, format!("this device may not use the account {asked}")))
            };
        }
        // A default it may no longer use is skipped, not a lockout.
        if let Some(default) = self.defaults.get(device).filter(|a| self.allows(device, a, regular)) {
            return Ok(default.clone());
        }
        let named = self.accounts_of(device);
        let any = self.lists(ANY, device);
        match (named.as_slice(), any) {
            ([only], false) => Ok(only.clone()),
            ([], false) => Err(refused(code::NOT_ALLOWED, "this device may not use any account here")),
            (choices, any) => {
                let mut choices = choices.join(", ");
                if any {
                    choices.push_str(if choices.is_empty() { "any regular account" } else { " or any regular account" });
                }
                Err(refused(code::CHOOSE_ACCOUNT, format!("choose an account ({choices}): add it in front of the ticket, as account@silol1…")))
            }
        }
    }

    /// Let `device` (labelled `name`) use `account`; whether it changed.
    ///
    /// # Errors
    /// A bad account name or device id.
    pub fn allow(&mut self, account: &str, device: &str, name: &str) -> anyhow::Result<bool> {
        if account != ANY && !valid_account(account) {
            bail!("{account:?} is not a user name");
        }
        let device = device_id(device)?;
        if self.lists(account, &device) {
            return Ok(false);
        }
        let name: String = name.chars().filter(|c| !c.is_control()).take(128).collect();
        let entry = if name.trim().is_empty() { device.clone() } else { format!("{device} {}", name.trim()) };
        let accounts = self.doc.entry("accounts").or_insert_with(|| json!({}));
        let Value::Object(accounts) = accounts else { bail!("\"accounts\" is not an object") };
        let account_entry = accounts.entry(account).or_insert_with(|| json!({}));
        let Value::Object(account_entry) = account_entry else { bail!("account {account:?} is not an object") };
        let devices = account_entry.entry("devices").or_insert_with(|| json!([]));
        let Value::Array(devices) = devices else { bail!("\"{account}\".devices is not a list") };
        devices.push(Value::String(entry));
        *self = Self::parse(&serde_json::to_vec(&self.doc)?)?;
        Ok(true)
    }

    /// Stop `device` using `account` (and drop a default naming it);
    /// whether it changed.
    ///
    /// # Errors
    /// A bad device id.
    pub fn deny(&mut self, account: &str, device: &str) -> anyhow::Result<bool> {
        let device = device_id(device)?;
        let mut changed = false;
        if let Some(Value::Array(devices)) = self.doc.get_mut("accounts").and_then(|a| a.get_mut(account)).and_then(|a| a.get_mut("devices")) {
            let before = devices.len();
            devices.retain(|d| d.as_str().and_then(|d| parse_entry(d).ok()).is_none_or(|d| d.id != device));
            changed = devices.len() != before;
        }
        if self.defaults.get(&device).is_some_and(|a| a == account)
            && let Some(Value::Object(defaults)) = self.doc.get_mut("defaults")
        {
            defaults.retain(|d, _| device_id(d).ok().as_deref() != Some(device.as_str()));
            changed = true;
        }
        if changed {
            *self = Self::parse(&serde_json::to_vec(&self.doc)?)?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn id() -> String {
        silo_transport::Identity::ephemeral().node_id()
    }

    fn regular(account: &str) -> bool {
        matches!(account, "alice" | "bob" | "carol")
    }

    fn policy(doc: Value) -> Policy {
        Policy::parse(&serde_json::to_vec(&doc).unwrap()).unwrap()
    }

    #[test]
    fn an_account_is_chosen_by_name_default_or_being_the_only_one() {
        let [a, b, c, d]: [String; 4] = std::array::from_fn(|_| id());
        let p = policy(json!({
            "version": 1,
            "accounts": {
                "alice": {"devices": [format!("{a} work laptop"), b.clone()]},
                "root": {"devices": [a.clone()]},
                "*": {"devices": [c.clone()]},
            },
            "defaults": {a.clone(): "alice"},
        }));
        let r: &dyn Fn(&str) -> bool = &regular;
        assert_eq!(p.choose(&a, None, r).unwrap(), "alice", "its default");
        assert_eq!(p.choose(&a, Some("root"), r).unwrap(), "root", "asked for by name");
        assert_eq!(p.choose(&b, None, r).unwrap(), "alice", "its only account");
        assert_eq!(p.choose(&b, Some("root"), r).unwrap_err().code, code::NOT_ALLOWED);
        // `*`: any regular account, never root or a system account.
        assert_eq!(p.choose(&c, Some("bob"), r).unwrap(), "bob");
        assert_eq!(p.choose(&c, Some("root"), r).unwrap_err().code, code::NOT_ALLOWED);
        assert_eq!(p.choose(&c, Some("daemon"), r).unwrap_err().code, code::NOT_ALLOWED);
        let choose = p.choose(&c, None, r).unwrap_err();
        assert_eq!(choose.code, code::CHOOSE_ACCOUNT);
        assert!(choose.message.contains("any regular account"), "{}", choose.message);
        assert_eq!(p.choose(&d, None, r).unwrap_err().code, code::NOT_ALLOWED, "an unknown device");
        assert_eq!(p.accounts()["alice"][0], Device { id: a.clone(), name: "work laptop".into() });
    }

    #[test]
    fn several_accounts_without_a_default_name_the_choices() {
        let a = id();
        let p = policy(json!({"accounts": {"alice": {"devices": [a.clone()]}, "root": {"devices": [a.clone()]}}, "defaults": {a.clone(): "bob"}}));
        let error = p.choose(&a, None, &regular).unwrap_err();
        assert_eq!(error.code, code::CHOOSE_ACCOUNT, "a default it may not use is skipped");
        assert!(error.message.contains("alice, root"), "{}", error.message);
    }

    #[test]
    fn bad_files_are_refused_whole() {
        for bad in [
            json!([]),
            json!({"version": 2}),
            json!({"accounts": {"bad name": {"devices": []}}}),
            json!({"accounts": {"alice": {"devices": ["not-a-key"]}}}),
            json!({"accounts": {"alice": {"devices": "x"}}}),
            json!({"defaults": {id(): "$(id)"}}),
        ] {
            assert!(Policy::parse(&serde_json::to_vec(&bad).unwrap()).is_err(), "{bad}");
        }
        assert!(Policy::parse(b"{").is_err());
        assert_eq!(Policy::parse(b"{}").unwrap().pairs(), Vec::<(String, String)>::new());
    }

    #[test]
    fn rewrites_keep_unknown_keys_and_are_private() {
        let dir = std::env::temp_dir().join(format!("silo-login-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.json");
        let (a, b) = (id(), id());
        let doc = json!({
            "version": 1,
            "comment": "kept",
            "accounts": {"alice": {"devices": [a.clone()], "note": "also kept"}},
            "defaults": {a.clone(): "alice"},
        });
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let mut p = Policy::load(&path).unwrap();
        assert!(p.allow("alice", &b, "phone").unwrap());
        assert!(!p.allow("alice", &b, "phone").unwrap(), "already there");
        assert!(p.allow("bob", &a, "").unwrap());
        assert!(p.deny("alice", &a).unwrap());
        assert!(!p.deny("alice", &a).unwrap());
        p.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["comment"], "kept");
        assert_eq!(saved["accounts"]["alice"]["note"], "also kept");
        assert_eq!(saved["accounts"]["alice"]["devices"], json!([format!("{b} phone")]));
        assert_eq!(saved["accounts"]["bob"]["devices"], json!([a.clone()]));
        assert_eq!(saved["defaults"], json!({}), "the default for a denied account goes");
        assert_eq!(Policy::load(&path).unwrap(), p);
        // A symlink in its place is replaced, not written through.
        let target = dir.join("elsewhere");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Policy::load(&path).is_err(), "not read through a symlink");
        p.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(Policy::load(&dir.join("missing.json")).unwrap(), Policy::empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
