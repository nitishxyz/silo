//! Grants, sealed in `<home>/state/grants.sealed` with the state store's
//! host key. A home from before grants keeps its paired devices: on first
//! open, `devices.sealed` becomes device grants and is removed (one-way).

use std::path::{Path, PathBuf};

use anyhow::Context;
use silo_proto::{
    access::{Grant, Rights, Scope, SecretKind, Who, secret_hash},
    control::{PairedDevice, RpcError},
};
use silo_store::HostKey;

const GRANTS_FILE: &str = "grants.sealed";
const GRANTS_AAD: &[u8] = b"silo grants";
const DEVICES_FILE: &str = "devices.sealed";
const DEVICES_AAD: &[u8] = b"silo devices";
/// An admitted hello rewrites the file for `last_seen` at most this often.
pub const LAST_SEEN_INTERVAL: u64 = 60;

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

pub struct Grants {
    key: HostKey,
    path: PathBuf,
    grants: Vec<Grant>,
    /// `last_seen` changed in memory since the file was written.
    seen_dirty: bool,
}

fn read(path: &Path, key: &HostKey, aad: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    match silo_store::read_sealed(path, key, aad) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(silo_store::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The device grant a paired device of before grants becomes.
fn device_grant(device: PairedDevice) -> Grant {
    Grant {
        id: new_id(),
        who: Who::Device { id: device.id },
        scope: Scope::Server,
        rights: Rights::Control,
        label: device.name,
        created_at: device.paired_at,
        created_by: String::new(),
        created_by_grant: None,
        expires_at: None,
        last_seen: device.last_seen,
    }
}

impl Grants {
    /// Load the grants in the state directory `dir`, converting the devices
    /// of a `devices.sealed` (written by a build before grants, e.g. one
    /// that kept serving after an aborted upgrade) that have no grant yet.
    pub fn open(dir: &Path, key: HostKey) -> anyhow::Result<Self> {
        let path = dir.join(GRANTS_FILE);
        let grants = match read(&path, &key, GRANTS_AAD)? {
            Some(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
            None => Vec::new(),
        };
        let mut grants = Self { key, path, grants, seen_dirty: false };
        let devices_path = dir.join(DEVICES_FILE);
        let Some(bytes) = read(&devices_path, &grants.key, DEVICES_AAD)? else { return Ok(grants) };
        let devices: Vec<PairedDevice> = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", devices_path.display()))?;
        let new: Vec<Grant> = devices.into_iter().filter(|d| grants.device(&d.id).is_none()).map(device_grant).collect();
        let count = new.len();
        grants.grants.extend(new);
        grants.save().map_err(|e| anyhow::anyhow!("{e}"))?;
        std::fs::remove_file(&devices_path).with_context(|| format!("remove {}", devices_path.display()))?;
        tracing::info!(devices = count, "paired devices converted to grants");
        Ok(grants)
    }

    fn save(&mut self) -> Result<(), RpcError> {
        let json = serde_json::to_vec(&self.grants).map_err(RpcError::internal)?;
        silo_store::write_sealed(&self.path, &self.key, GRANTS_AAD, &json).map_err(RpcError::internal)?;
        self.seen_dirty = false;
        Ok(())
    }

    /// Write `last_seen` updates not yet on disk.
    pub fn flush(&mut self) {
        if self.seen_dirty
            && let Err(error) = self.save()
        {
            tracing::warn!("record last_seen: {error}");
        }
    }

    pub fn all(&self) -> &[Grant] {
        &self.grants
    }

    pub fn get(&self, id: &str) -> Option<&Grant> {
        self.grants.iter().find(|g| g.id == id)
    }

    /// The unexpired device grant of `device`.
    pub fn device(&self, device: &str) -> Option<&Grant> {
        let now = unix_secs();
        self.grants.iter().find(|g| matches!(&g.who, Who::Device { id } if id == device) && !g.expired(now))
    }

    /// The unexpired grant of a bearer secret of `kind`.
    pub fn secret(&self, kind: SecretKind, secret: &[u8; silo_proto::access::SECRET_LEN]) -> Option<&Grant> {
        let (hash, now) = (secret_hash(secret), unix_secs());
        self.grants.iter().find(|g| matches!(&g.who, Who::Secret { kind: k, hash: h } if *k == kind && *h == hash) && !g.expired(now))
    }

    /// Record an admitted hello on grant `id` (debounced write).
    pub fn seen(&mut self, id: &str) {
        let now = unix_secs();
        let Some(grant) = self.grants.iter_mut().find(|g| g.id == id) else { return };
        let stale = now.saturating_sub(grant.last_seen) >= LAST_SEEN_INTERVAL;
        grant.last_seen = now;
        self.seen_dirty = true;
        if stale {
            self.flush();
        }
    }

    /// Store a new grant.
    pub fn add(&mut self, grant: Grant) -> Result<(), RpcError> {
        self.grants.push(grant);
        if let Err(error) = self.save() {
            self.grants.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Remove the grants `remove` selects and, transitively, every link
    /// and token one of them created (`created_by_grant`); the removed
    /// ones. Device grants are never removed this way, so the cascade stops
    /// at them: removing one device must not cut off the devices it paired.
    pub fn remove_where(&mut self, remove: impl Fn(&Grant) -> bool) -> Result<Vec<Grant>, RpcError> {
        let before = self.grants.clone();
        let (mut gone, mut kept): (Vec<Grant>, Vec<Grant>) = std::mem::take(&mut self.grants).into_iter().partition(|g| remove(g));
        let mut from = 0;
        while from < gone.len() {
            let ids: Vec<String> = gone[from..].iter().map(|g| g.id.clone()).collect();
            from = gone.len();
            let (created, rest): (Vec<Grant>, Vec<Grant>) =
                kept.into_iter().partition(|g| matches!(g.who, Who::Secret { .. }) && g.created_by_grant.as_ref().is_some_and(|c| ids.contains(c)));
            gone.extend(created);
            kept = rest;
        }
        self.grants = kept;
        if gone.is_empty() {
            return Ok(gone);
        }
        if let Err(error) = self.save() {
            self.grants = before;
            return Err(error);
        }
        Ok(gone)
    }

    /// Paired devices as `pair.list` shows them.
    pub fn devices(&self) -> Vec<PairedDevice> {
        self.grants
            .iter()
            .filter_map(|g| match &g.who {
                Who::Device { id } => Some(PairedDevice { id: id.clone(), name: g.label.clone(), paired_at: g.created_at, last_seen: g.last_seen }),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_sealed_converts_once_to_the_same_devices() {
        let home = PathBuf::from(format!("/tmp/silo-grants-convert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join("state");
        let key = || HostKey::load_or_create(&dir).unwrap();
        key();
        let devices = vec![
            PairedDevice { id: "aa".into(), name: "work laptop".into(), paired_at: 10, last_seen: 20 },
            PairedDevice { id: "bb".into(), name: "phone".into(), paired_at: 30, last_seen: 40 },
        ];
        silo_store::write_sealed(dir.join(DEVICES_FILE), &key(), DEVICES_AAD, &serde_json::to_vec(&devices).unwrap()).unwrap();
        let grants = Grants::open(&dir, key()).unwrap();
        assert_eq!(grants.devices(), devices);
        assert!(grants.all().iter().all(|g| g.scope == Scope::Server && g.rights == Rights::Control && g.expires_at.is_none()));
        assert!(!dir.join(DEVICES_FILE).exists(), "converted once");
        let again = Grants::open(&dir, key()).unwrap();
        assert_eq!(again.devices(), devices);
        assert_eq!(again.all(), grants.all());
        // An older build kept serving and paired another device: only that
        // one is added.
        let tablet = PairedDevice { id: "cc".into(), name: "tablet".into(), paired_at: 50, last_seen: 60 };
        let rewritten = [devices.clone(), vec![tablet]].concat();
        silo_store::write_sealed(dir.join(DEVICES_FILE), &key(), DEVICES_AAD, &serde_json::to_vec(&rewritten).unwrap()).unwrap();
        let merged = Grants::open(&dir, key()).unwrap();
        assert_eq!(merged.devices(), rewritten);
        assert_eq!(merged.all()[..2], *grants.all());
        assert!(!dir.join(DEVICES_FILE).exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn removing_a_grant_removes_the_links_and_tokens_it_created_transitively() {
        let home = PathBuf::from(format!("/tmp/silo-grants-cascade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = home.join("state");
        let mut grants = Grants::open(&dir, HostKey::load_or_create(&dir).unwrap()).unwrap();
        let grant = |id: &str, device: bool, by: Option<&str>| Grant {
            id: id.into(),
            who: if device { Who::Device { id: id.into() } } else { Who::Secret { kind: SecretKind::Link, hash: id.into() } },
            scope: Scope::Server,
            rights: Rights::View,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            created_by_grant: by.map(Into::into),
            expires_at: None,
            last_seen: 0,
        };
        // Device a created link b, which created c; a paired device f, which
        // created token g. d is the owner's; e is from before creators were
        // kept.
        for g in [
            grant("a", true, None),
            grant("b", false, Some("a")),
            grant("c", false, Some("b")),
            grant("d", false, None),
            grant("e", false, None),
            grant("f", true, Some("a")),
            grant("g", false, Some("f")),
        ] {
            grants.add(g).unwrap();
        }
        let gone: Vec<String> = grants.remove_where(|g| g.id == "a").unwrap().into_iter().map(|g| g.id).collect();
        assert_eq!(gone, ["a", "b", "c"]);
        let kept: Vec<&str> = grants.all().iter().map(|g| g.id.as_str()).collect();
        assert_eq!(kept, ["d", "e", "f", "g"], "the paired device and what it created stay");
        assert_eq!(grants.all()[2].created_by_grant.as_deref(), Some("a"), "its provenance is kept");
        let reopened = Grants::open(&dir, HostKey::load_or_create(&dir).unwrap()).unwrap();
        assert_eq!(reopened.all(), grants.all(), "saved");
        // Removing f directly takes g with it.
        let gone: Vec<String> = grants.remove_where(|g| g.id == "f").unwrap().into_iter().map(|g| g.id).collect();
        assert_eq!(gone, ["f", "g"]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
