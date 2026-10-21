//! Remote access: grants (see [`crate::grants`]), memory-only pairing
//! invites, and live remote connections with the grant each speaks for.

use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Context;
use silo_proto::{
    access::{Grant, Rights, Scope, SecretKind, Who},
    control::{HostOffer, InvitePurpose, OfferSource, PairedDevice, RpcError},
    device::DeviceProof,
};
use silo_store::HostKey;
use silo_transport::{INVITE_SECRET_LEN, PairTicket, RemoteConn};

use crate::{
    grants::{Grants, new_id, unix_secs},
    persist,
};

/// Longest pairing invite lifetime.
pub const MAX_INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Who is on the other end of a control or data stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peer {
    /// Same-uid unix socket client, with its process id when known.
    Local { pid: Option<i32> },
    /// An iroh connection, authenticated by its endpoint id.
    Remote { endpoint_id: String, conn_id: u64 },
}

impl Peer {
    pub fn conn_id(&self) -> Option<u64> {
        match self {
            Peer::Local { .. } => None,
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
    /// A paired device, with its grant.
    Device(Box<Grant>),
    /// A redeemed join invite: may only call `host.offer`, once.
    Join { invite_id: String },
}

/// A live remote connection.
struct Live {
    endpoint: String,
    /// The grant it was admitted with; revoking it closes the connection.
    grant: Option<String>,
    conn: RemoteConn,
}

pub struct Auth {
    grants: Grants,
    invites: HashMap<[u8; INVITE_SECRET_LEN], Invite>,
    /// Live remote connections by `RemoteConn::id`.
    live: HashMap<u64, Live>,
}

/// Longest offered host name kept, in chars.
const MAX_OFFER_NAME: usize = 128;

/// A [`HostOffer`] for `ticket` if it is a valid pairing ticket.
pub fn host_offer(ticket: &str, name: &str, source: OfferSource) -> Option<HostOffer> {
    let endpoint_id = PairTicket::decode(ticket).ok()?.addr.id.to_string();
    let name = name.trim().chars().filter(|c| !c.is_control()).take(MAX_OFFER_NAME).collect();
    Some(HostOffer { ticket: ticket.trim().to_owned(), name, endpoint_id, source })
}

/// The device a remote endpoint authenticates as: the one whose `proof`
/// it presents, else itself. `server_id` is this server's endpoint id.
pub fn device_of(server_id: &str, endpoint_id: &str, proof: Option<&DeviceProof>) -> Result<String, RpcError> {
    let Some(proof) = proof else { return Ok(endpoint_id.to_owned()) };
    let parse = |id: &str| id.parse::<silo_transport::EndpointId>().map_err(RpcError::internal);
    let (server, endpoint) = (parse(server_id)?, parse(endpoint_id)?);
    silo_transport::verify_proof(&proof.device, &proof.signature, server, endpoint)
        .map(|device| device.to_string())
        .map_err(|e| RpcError::unauthorized(format!("{e:#}")))
}

impl Auth {
    /// Load the grants sealed with the state store's host key.
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let dir = persist::state_dir(home);
        let key = HostKey::load_or_create(&dir).with_context(|| format!("host key in {}", dir.display()))?;
        Ok(Self { grants: Grants::open(&dir, key)?, invites: HashMap::new(), live: HashMap::new() })
    }

    /// Write `last_seen` updates not yet on disk (at shutdown or handoff).
    pub fn flush(&mut self) {
        self.grants.flush();
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
        let id = new_id();
        self.invites.insert(secret, Invite { id: id.clone(), purpose, expires });
        Ok((secret, id))
    }

    /// Admit `endpoint_id` if paired, else redeem `invite`, consuming it: a
    /// device invite creates the device's grant, a join invite nothing.
    pub fn authorize(&mut self, endpoint_id: &str, name: &str, invite: Option<&str>) -> Result<Admission, RpcError> {
        if let Some(grant) = self.grants.device(endpoint_id).cloned() {
            self.grants.seen(&grant.id);
            return Ok(Admission::Device(Box::new(grant)));
        }
        let invite = invite.ok_or_else(|| RpcError::unauthorized("unknown device; pair it with an invite"))?;
        let ticket = PairTicket::decode(invite).map_err(|e| RpcError::unauthorized(format!("{e:#}")))?;
        let invite = self.invites.remove(&ticket.secret).filter(|i| i.expires > Instant::now());
        let invite = invite.ok_or_else(|| RpcError::unauthorized("invalid, expired or already used invite"))?;
        if invite.purpose == InvitePurpose::Join {
            tracing::info!(peer = endpoint_id, name, "redeemed a join invite");
            return Ok(Admission::Join { invite_id: invite.id });
        }
        let now = unix_secs();
        let grant = Grant {
            id: new_id(),
            who: Who::Device { id: endpoint_id.to_owned() },
            scope: Scope::Server,
            rights: Rights::Control,
            label: name.to_owned(),
            created_at: now,
            created_by: String::new(),
            expires_at: None,
            last_seen: now,
        };
        if let Err(error) = self.grants.add(grant.clone()) {
            self.invites.insert(ticket.secret, invite);
            return Err(error);
        }
        tracing::info!(device = endpoint_id, name, "paired a device");
        Ok(Admission::Device(Box::new(grant)))
    }

    /// The unexpired grant of a bearer secret's text of `kind`, recording
    /// the hello.
    pub fn redeem_secret(&mut self, kind: SecretKind, text: &str) -> Result<Grant, RpcError> {
        let refused = || RpcError::unauthorized("invalid, expired or revoked grant");
        let (found, secret) = silo_proto::access::decode_secret(text).ok_or_else(refused)?;
        if found != kind {
            return Err(refused());
        }
        let grant = self.grants.secret(kind, &secret).cloned().ok_or_else(refused)?;
        self.grants.seen(&grant.id);
        Ok(grant)
    }

    /// The unexpired device grant of `device`.
    pub fn device_grant(&self, device: &str) -> Option<Grant> {
        self.grants.device(device).cloned()
    }

    pub fn get(&self, id: &str) -> Option<Grant> {
        self.grants.get(id).cloned()
    }

    pub fn devices(&self) -> Vec<PairedDevice> {
        self.grants.devices()
    }

    pub fn grants(&self) -> Vec<Grant> {
        self.grants.all().to_vec()
    }

    pub fn add_grant(&mut self, grant: Grant) -> Result<(), RpcError> {
        self.grants.add(grant)
    }

    /// Remove the grants `remove` selects and close their remote
    /// connections; the removed ones (their local clients are the caller's
    /// to close).
    pub fn remove_where(&mut self, remove: impl Fn(&Grant) -> bool) -> Result<Vec<Grant>, RpcError> {
        let gone = self.grants.remove_where(remove)?;
        for live in self.live.values() {
            if live.grant.as_ref().is_some_and(|g| gone.iter().any(|x| &x.id == g)) {
                live.conn.close(b"revoked");
            }
        }
        Ok(gone)
    }

    /// Forget the paired device `id` (its grant); see [`Self::remove_where`].
    pub fn revoke_device(&mut self, id: &str) -> Result<Vec<Grant>, RpcError> {
        let grant = self.grants.device(id).ok_or_else(|| RpcError::not_found(format!("device {id}")))?.id.clone();
        self.remove_where(|g| g.id == grant)
    }

    pub fn add_live(&mut self, endpoint_id: String, conn: RemoteConn) {
        self.live.insert(conn.id(), Live { endpoint: endpoint_id, grant: None, conn });
    }

    /// The live connection `conn_id` speaks for grant `grant`: revoking
    /// it closes the connection.
    pub fn bind(&mut self, conn_id: u64, grant: &str) {
        if let Some(live) = self.live.get_mut(&conn_id) {
            live.grant = Some(grant.to_owned());
        }
    }

    /// The endpoint on the other end of live connection `conn_id`.
    pub fn endpoint(&self, conn_id: u64) -> Option<&str> {
        self.live.get(&conn_id).map(|l| l.endpoint.as_str())
    }

    pub fn remove_live(&mut self, conn_id: u64) {
        self.live.remove(&conn_id);
    }

    /// The grant a tunnel from `endpoint` runs under, if it holds control
    /// at server scope: the endpoint's own device grant, or the grant of a
    /// live connection it opened.
    pub fn tunnel_grant(&self, endpoint: &str) -> Option<Grant> {
        let control = |g: &Grant| g.scope == Scope::Server && g.rights >= Rights::Control && !g.expired(unix_secs());
        if let Some(grant) = self.grants.device(endpoint).filter(|g| control(g)) {
            return Some(grant.clone());
        }
        self.live.values().filter(|l| l.endpoint == endpoint).filter_map(|l| self.grants.get(l.grant.as_deref()?)).find(|g| control(g)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
        let path = persist::state_dir(&home).join("grants.sealed");
        let written = std::fs::read(&path).unwrap();

        auth.authorize("dev", "Dev", None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), written, "a fresh last_seen is not rewritten");
        auth.flush();
        assert_ne!(std::fs::read(&path).unwrap(), written, "flush writes it");
        let flushed = std::fs::read(&path).unwrap();
        auth.flush();
        assert_eq!(std::fs::read(&path).unwrap(), flushed, "nothing left to flush");
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
        assert!(auth.devices().is_empty() && auth.device_grant("dev").is_none());
        assert_eq!(auth.authorize("dev", "Dev", Some(&invite)).unwrap_err().code, RpcError::UNAUTHORIZED);
        assert_eq!(auth.authorize("dev", "Dev", None).unwrap_err().code, RpcError::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn secrets_redeem_only_their_kind_until_removed() {
        let home = PathBuf::from(format!("/tmp/silo-auth-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut auth = Auth::open(&home).unwrap();
        let secret = [3u8; 32];
        let grant = Grant {
            id: new_id(),
            who: Who::Secret { kind: SecretKind::Token, hash: silo_proto::access::secret_hash(&secret) },
            scope: Scope::Server,
            rights: Rights::View,
            label: String::new(),
            created_at: 0,
            created_by: String::new(),
            expires_at: None,
            last_seen: 0,
        };
        auth.add_grant(grant.clone()).unwrap();
        let token = silo_proto::access::encode_secret(SecretKind::Token, &secret);
        assert_eq!(auth.redeem_secret(SecretKind::Token, &token).unwrap().id, grant.id);
        let link = silo_proto::access::encode_secret(SecretKind::Link, &secret);
        assert_eq!(auth.redeem_secret(SecretKind::Link, &link).unwrap_err().code, RpcError::UNAUTHORIZED);
        assert_eq!(auth.remove_where(|g| g.id == grant.id).unwrap().len(), 1);
        assert_eq!(auth.redeem_secret(SecretKind::Token, &token).unwrap_err().code, RpcError::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&home);
    }
}
