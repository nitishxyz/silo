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
        expires_at: None,
        last_seen: device.last_seen,
    }
}

impl Grants {
    /// Load the grants in the state directory `dir`, converting
    /// `devices.sealed` if there are none yet.
    pub fn open(dir: &Path, key: HostKey) -> anyhow::Result<Self> {
        let path = dir.join(GRANTS_FILE);
        if let Some(bytes) = read(&path, &key, GRANTS_AAD)? {
            let grants = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
            return Ok(Self { key, path, grants, seen_dirty: false });
        }
        let devices_path = dir.join(DEVICES_FILE);
        let devices: Vec<PairedDevice> = match read(&devices_path, &key, DEVICES_AAD)? {
            Some(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", devices_path.display()))?,
            None => return Ok(Self { key, path, grants: Vec::new(), seen_dirty: false }),
        };
        let mut grants = Self { key, path, grants: devices.into_iter().map(device_grant).collect(), seen_dirty: false };
        grants.save().map_err(|e| anyhow::anyhow!("{e}"))?;
        std::fs::remove_file(&devices_path).with_context(|| format!("remove {}", devices_path.display()))?;
        tracing::info!(devices = grants.grants.len(), "paired devices converted to grants");
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

    /// Remove the grants `remove` selects; the removed ones.
    pub fn remove_where(&mut self, remove: impl Fn(&Grant) -> bool) -> Result<Vec<Grant>, RpcError> {
        let before = self.grants.clone();
        let (gone, kept): (Vec<Grant>, Vec<Grant>) = std::mem::take(&mut self.grants).into_iter().partition(|g| remove(g));
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
        let _ = std::fs::remove_dir_all(&home);
    }
}
