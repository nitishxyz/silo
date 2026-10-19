//! Remote access: paired-device grants (sealed under `<home>/state`),
//! memory-only pairing invites, and live remote connections.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context;
use silo_proto::control::{HostOffer, InvitePurpose, OfferSource, PairedDevice, RpcError};
use silo_store::HostKey;
use silo_transport::{INVITE_SECRET_LEN, PairTicket, RemoteConn};

use crate::persist;

const DEVICES_FILE: &str = "devices.sealed";
const DEVICES_AAD: &[u8] = b"silo devices";
/// Longest pairing invite lifetime.
pub const MAX_INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A paired hello rewrites the grants file for `last_seen` at most this often.
const LAST_SEEN_INTERVAL: u64 = 60;

/// Who is on the other end of a control or data stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peer {
    /// Same-uid unix socket client: the owner.
    Local,
    /// An iroh connection, authenticated by its endpoint id.
    Remote { endpoint_id: String, conn_id: u64 },
}

impl Peer {
    pub fn conn_id(&self) -> Option<u64> {
        match self {
            Peer::Local => None,
            Peer::Remote { conn_id, .. } => Some(*conn_id),
        }
    }
}

/// An outstanding invite.
#[derive(Clone)]
struct Invite {
    id: String,
    purpose: InvitePurpose,
    expires: Instant,
}

/// What an authorized remote control hello may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// A paired device.
    Device,
    /// A redeemed join invite: may only call `host.offer`, once.
    Join { invite_id: String },
}

pub struct Auth {
    key: HostKey,
    path: PathBuf,
    devices: Vec<PairedDevice>,
    invites: HashMap<[u8; INVITE_SECRET_LEN], Invite>,
    /// Live remote connections by `RemoteConn::id`, with their endpoint id.
    live: HashMap<u64, (String, RemoteConn)>,
    /// `last_seen` changed in memory since the grants file was written.
    seen_dirty: bool,
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Longest offered host name kept, in chars.
const MAX_OFFER_NAME: usize = 128;

/// A [`HostOffer`] for `ticket` if it is a valid pairing ticket.
pub fn host_offer(ticket: &str, name: &str, source: OfferSource) -> Option<HostOffer> {
    let endpoint_id = PairTicket::decode(ticket).ok()?.addr.id.to_string();
    let name = name.trim().chars().filter(|c| !c.is_control()).take(MAX_OFFER_NAME).collect();
    Some(HostOffer { ticket: ticket.trim().to_owned(), name, endpoint_id, source })
}

impl Auth {
    /// Load the grants sealed with the state store's host key.
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let dir = persist::state_dir(home);
        let key = HostKey::load_or_create(&dir).with_context(|| format!("host key in {}", dir.display()))?;
        let path = dir.join(DEVICES_FILE);
        let devices = match silo_store::read_sealed(&path, &key, DEVICES_AAD) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
            Err(silo_store::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self { key, path, devices, invites: HashMap::new(), live: HashMap::new(), seen_dirty: false })
    }

    fn save(&mut self) -> Result<(), RpcError> {
        let json = serde_json::to_vec(&self.devices).map_err(RpcError::internal)?;
        silo_store::write_sealed(&self.path, &self.key, DEVICES_AAD, &json).map_err(RpcError::internal)?;
        self.seen_dirty = false;
        Ok(())
    }

    /// Write `last_seen` updates not yet on disk (at shutdown or handoff).
    pub fn flush(&mut self) {
        if self.seen_dirty
            && let Err(error) = self.save()
        {
            tracing::warn!("record last_seen: {error}");
        }
    }

    /// A single-use invite valid for `ttl` (1 s ..= [`MAX_INVITE_TTL`]):
    /// its secret and id.
    pub fn create_invite(&mut self, ttl: Duration, purpose: InvitePurpose) -> Result<([u8; INVITE_SECRET_LEN], String), RpcError> {
        if ttl.is_zero() || ttl > MAX_INVITE_TTL {
            return Err(RpcError::bad_request(format!("ttl_secs must be between 1 and {}", MAX_INVITE_TTL.as_secs())));
        }
        let now = Instant::now();
        let expires = now.checked_add(ttl).ok_or_else(|| RpcError::bad_request("ttl_secs is too large"))?;
        self.invites.retain(|_, invite| invite.expires > now);
        let secret = rand::random::<[u8; INVITE_SECRET_LEN]>();
        let id = format!("{:016x}", rand::random::<u64>());
        self.invites.insert(secret, Invite { id: id.clone(), purpose, expires });
        Ok((secret, id))
    }

    /// Admit `endpoint_id` if paired, else redeem `invite`, consuming it: a
    /// device invite records the grant, a join invite records nothing.
    pub fn authorize(&mut self, endpoint_id: &str, name: &str, invite: Option<&str>) -> Result<Admission, RpcError> {
        let now = unix_secs();
        if let Some(device) = self.devices.iter_mut().find(|d| d.id == endpoint_id) {
            let stale = now.saturating_sub(device.last_seen) >= LAST_SEEN_INTERVAL;
            device.last_seen = now;
            self.seen_dirty = true;
            if stale {
                self.flush();
            }
            return Ok(Admission::Device);
        }
        let invite = invite.ok_or_else(|| RpcError::unauthorized("unknown device; pair it with an invite"))?;
        let ticket = PairTicket::decode(invite).map_err(|e| RpcError::unauthorized(format!("{e:#}")))?;
        let invite = self.invites.remove(&ticket.secret).filter(|i| i.expires > Instant::now());
        let invite = invite.ok_or_else(|| RpcError::unauthorized("invalid, expired or already used invite"))?;
        if invite.purpose == InvitePurpose::Join {
            tracing::info!(peer = endpoint_id, name, "redeemed a join invite");
            return Ok(Admission::Join { invite_id: invite.id });
        }
        self.devices.push(PairedDevice { id: endpoint_id.to_owned(), name: name.to_owned(), paired_at: now, last_seen: now });
        if let Err(error) = self.save() {
            self.devices.pop();
            self.invites.insert(ticket.secret, invite);
            return Err(error);
        }
        tracing::info!(device = endpoint_id, name, "paired a device");
        Ok(Admission::Device)
    }

    /// Whether `endpoint_id` holds a grant (read-only; no `last_seen`).
    pub fn is_paired(&self, endpoint_id: &str) -> bool {
        self.devices.iter().any(|d| d.id == endpoint_id)
    }

    pub fn devices(&self) -> Vec<PairedDevice> {
        self.devices.clone()
    }

    /// Forget a device and close its live connections.
    pub fn revoke(&mut self, id: &str) -> Result<(), RpcError> {
        let index = self.devices.iter().position(|d| d.id == id).ok_or_else(|| RpcError::not_found(format!("device {id}")))?;
        let device = self.devices.remove(index);
        if let Err(error) = self.save() {
            self.devices.insert(index, device);
            return Err(error);
        }
        for (endpoint, conn) in self.live.values() {
            if endpoint == id {
                conn.close(b"revoked");
            }
        }
        Ok(())
    }

    pub fn add_live(&mut self, endpoint_id: String, conn: RemoteConn) {
        self.live.insert(conn.id(), (endpoint_id, conn));
    }

    pub fn remove_live(&mut self, conn_id: u64) {
        self.live.remove(&conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_seen_writes_are_debounced_and_flushed() {
        let home = PathBuf::from(format!("/tmp/silo-auth-debounce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut auth = Auth::open(&home).unwrap();
        assert!(auth.create_invite(Duration::ZERO, InvitePurpose::Device).is_err());
        assert!(auth.create_invite(MAX_INVITE_TTL + Duration::from_secs(1), InvitePurpose::Device).is_err());
        let (secret, _) = auth.create_invite(Duration::from_secs(60), InvitePurpose::Device).unwrap();
        let id = silo_transport::Identity::load_or_create(home.join("peer")).unwrap().id();
        let invite = PairTicket { addr: silo_transport::EndpointAddr::new(id), secret }.encode();
        auth.authorize("dev", "Dev", Some(&invite)).unwrap();
        let written = std::fs::read(&auth.path).unwrap();

        auth.authorize("dev", "Dev", None).unwrap();
        assert_eq!(std::fs::read(&auth.path).unwrap(), written, "a fresh last_seen is not rewritten");
        auth.flush();
        assert_ne!(std::fs::read(&auth.path).unwrap(), written, "flush writes it");
        let flushed = std::fs::read(&auth.path).unwrap();
        auth.flush();
        assert_eq!(std::fs::read(&auth.path).unwrap(), flushed, "nothing left to flush");
        assert_eq!(Auth::open(&home).unwrap().devices().len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn join_invites_pair_nothing_and_are_single_use() {
        let home = PathBuf::from(format!("/tmp/silo-auth-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut auth = Auth::open(&home).unwrap();
        let (secret, id) = auth.create_invite(Duration::from_secs(60), InvitePurpose::Join).unwrap();
        let peer = silo_transport::Identity::load_or_create(home.join("peer")).unwrap().id();
        let invite = PairTicket { addr: silo_transport::EndpointAddr::new(peer), secret }.encode();
        assert_eq!(auth.authorize("dev", "Dev", Some(&invite)).unwrap(), Admission::Join { invite_id: id });
        assert!(auth.devices().is_empty() && !auth.is_paired("dev"));
        assert_eq!(auth.authorize("dev", "Dev", Some(&invite)).unwrap_err().code, RpcError::UNAUTHORIZED);
        assert_eq!(auth.authorize("dev", "Dev", None).unwrap_err().code, RpcError::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&home);
    }
}
