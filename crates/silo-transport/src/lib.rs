//! silo transport: unix + iroh connections, identity, pairing tickets.

mod identity;
mod net;
mod stream;
mod ticket;
pub mod tunnel;

pub use identity::{Identity, verify_proof};
pub use iroh::endpoint::{Path, PathList};
pub use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};
pub use net::{CTL_ALPN, CtlIncoming, Net, NetOptions, RelayConfig, RemoteConn, TUNNEL_ALPN, TunnelIncoming, Unreachable};
pub use stream::{AsyncStream, BiStream, Conn, Stream};
pub use ticket::{INVITE_SECRET_LEN, PairTicket, TICKET_PREFIX};
