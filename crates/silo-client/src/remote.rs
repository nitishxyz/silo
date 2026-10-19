//! Remote (iroh) connections. Never inspects or spawns a local server.

use std::path::Path;

use anyhow::Context;
use silo_proto::BUILD;
use silo_transport::{Conn, EndpointAddr, Identity, Net, PairTicket};

use crate::{Control, Events};

/// Load or create the client's device identity in `dir` (the caller's
/// state dir, e.g. `$SILO_HOME/client`).
///
/// # Errors
/// Unreadable or invalid key material.
pub fn client_identity(dir: &Path) -> anyhow::Result<Identity> {
    Identity::load_or_create(dir)
}

impl Control {
    /// Dial `addr` and complete the hello. `invite` is only needed for
    /// first-time pairing.
    ///
    /// # Errors
    /// Dial or handshake failure; a refused device is
    /// [`Unauthorized`](crate::Unauthorized), another build is
    /// [`BuildMismatch`](crate::BuildMismatch).
    pub async fn connect_remote(net: &Net, addr: EndpointAddr, client_name: &str, passive: bool, invite: Option<&PairTicket>) -> anyhow::Result<(Self, Events)> {
        let rc = net.dial(addr).await.context("dial server")?;
        let invite = invite.map(PairTicket::encode);
        Self::handshake(Conn::Iroh(rc), client_name, passive, BUILD, invite).await
    }
}

/// Redeem `ticket` and connect as a newly paired device.
///
/// # Errors
/// See [`Control::connect_remote`].
pub async fn pair(net: &Net, ticket: &PairTicket, client_name: &str) -> anyhow::Result<(Control, Events)> {
    Control::connect_remote(net, ticket.addr.clone(), client_name, false, Some(ticket)).await
}
