use std::path::Path;

use anyhow::{Context, Result, bail};
use data_encoding::HEXLOWER;
use iroh::{EndpointId, SecretKey, Signature};
use silo_store::HostKey;

/// Domain separation for device proofs.
const PROOF_CONTEXT: &[u8] = b"silo/device-proof/1";

fn proof_message(server: EndpointId, endpoint: EndpointId) -> Vec<u8> {
    [PROOF_CONTEXT, server.as_bytes(), endpoint.as_bytes()].concat()
}

/// The device whose key made `signature` (see [`Identity::prove`]) for
/// `endpoint` to speak for it to `server`.
///
/// # Errors
/// An invalid device id or signature, or one not for `server` and
/// `endpoint`.
pub fn verify_proof(device: &str, signature: &str, server: EndpointId, endpoint: EndpointId) -> Result<EndpointId> {
    let device: EndpointId = device.parse().context("invalid device id")?;
    let bytes = HEXLOWER.decode(signature.as_bytes()).context("invalid signature encoding")?;
    let Ok(bytes) = <[u8; Signature::LENGTH]>::try_from(bytes.as_slice()) else {
        bail!("invalid signature length");
    };
    device.verify(&proof_message(server, endpoint), &Signature::from_bytes(&bytes)).context("the device proof does not verify")?;
    Ok(device)
}

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
        let key = HostKey::load_or_create(dir).with_context(|| format!("load iroh identity from {}", dir.display()))?;
        Ok(Identity { secret: SecretKey::from_bytes(key.as_bytes()) })
    }

    /// A fresh key held only in memory: an endpoint of this process's own.
    #[must_use]
    pub fn ephemeral() -> Identity {
        Identity { secret: SecretKey::generate() }
    }

    /// This key's signature (lowercase hex) stating that `endpoint` speaks
    /// for it to `server`; see [`verify_proof`].
    #[must_use]
    pub fn prove(&self, server: EndpointId, endpoint: EndpointId) -> String {
        HEXLOWER.encode(&self.secret.sign(&proof_message(server, endpoint)).to_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_bind_device_server_and_endpoint() {
        let (device, server, endpoint, other) = (Identity::ephemeral(), Identity::ephemeral(), Identity::ephemeral(), Identity::ephemeral());
        let proof = device.prove(server.id(), endpoint.id());
        assert_eq!(verify_proof(&device.node_id(), &proof, server.id(), endpoint.id()).unwrap(), device.id());
        assert!(verify_proof(&device.node_id(), &proof, other.id(), endpoint.id()).is_err(), "another server");
        assert!(verify_proof(&device.node_id(), &proof, server.id(), other.id()).is_err(), "another endpoint");
        assert!(verify_proof(&other.node_id(), &proof, server.id(), endpoint.id()).is_err(), "another device");
        let forged = other.prove(server.id(), endpoint.id());
        assert!(verify_proof(&device.node_id(), &forged, server.id(), endpoint.id()).is_err(), "signed by another key");
        assert!(verify_proof(&device.node_id(), &proof[2..], server.id(), endpoint.id()).is_err());
        assert!(verify_proof("nope", &proof, server.id(), endpoint.id()).is_err());
        assert_ne!(Identity::ephemeral().id(), Identity::ephemeral().id());
    }
}
