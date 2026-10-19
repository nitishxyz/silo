use std::path::Path;

use anyhow::{Context, Result};
use iroh::{EndpointId, SecretKey};
use silo_store::HostKey;

/// Persistent iroh identity. Stored with the same rules as the snapshot
/// host key (32 random bytes, 0600, no symlinks) but in its own directory.
#[derive(Clone)]
pub struct Identity {
    secret: SecretKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.node_id())
    }
}

impl Identity {
    /// Load `dir/key`, creating it if absent. `dir` must be dedicated to
    /// the iroh identity (e.g. `<home>/state/iroh`), never the snapshot key dir.
    pub fn load_or_create(dir: impl AsRef<Path>) -> Result<Identity> {
        let dir = dir.as_ref();
        let key = HostKey::load_or_create(dir)
            .with_context(|| format!("load iroh identity from {}", dir.display()))?;
        Ok(Identity {
            secret: SecretKey::from_bytes(key.as_bytes()),
        })
    }

    pub fn id(&self) -> EndpointId {
        self.secret.public()
    }

    /// Public endpoint id as its canonical string form.
    pub fn node_id(&self) -> String {
        self.id().to_string()
    }

    pub(crate) fn secret(&self) -> &SecretKey {
        &self.secret
    }
}
