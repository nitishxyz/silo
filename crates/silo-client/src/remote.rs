//! Remote (iroh) connections. Never inspects or spawns a local server.

use std::path::Path;

use anyhow::Context;
use silo_proto::{
    BUILD,
    control::{Empty, Method},
};
use silo_transport::{Conn, EndpointAddr, Identity, Net, PairTicket, RemoteConn};

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
    /// [`IncompatibleProtocol`](crate::IncompatibleProtocol).
    pub async fn connect_remote(
        net: &Net,
        addr: EndpointAddr,
        client_name: &str,
        passive: bool,
        invite: Option<&PairTicket>,
    ) -> anyhow::Result<(Self, Events)> {
        let rc = net.dial(addr).await.context("dial server")?;
        Self::over_remote(rc, client_name, passive, invite).await
    }

    /// [`Control::connect_remote`] over `rc`, a connection just dialed.
    ///
    /// # Errors
    /// See [`Control::connect_remote`].
    pub async fn over_remote(rc: RemoteConn, client_name: &str, passive: bool, invite: Option<&PairTicket>) -> anyhow::Result<(Self, Events)> {
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

/// Redeem the join invite `join` and offer this machine's server, reachable
/// with pairing ticket `offer` (`silo1…`), under `name`. The inviting
/// server raises `host.offered`.
///
/// # Errors
/// See [`Control::connect_remote`]; a refused offer.
pub async fn offer_host(net: &Net, join: &PairTicket, offer: &str, name: &str) -> anyhow::Result<()> {
    let (control, _events) = Control::connect_remote(net, join.addr.clone(), name, false, Some(join)).await?;
    control.call::<Empty>(Method::HostOffer { ticket: offer.to_owned(), name: name.to_owned() }).await?;
    Ok(())
}
