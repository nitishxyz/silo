//! silo transport: unix + iroh connections, identity, pairing tickets.

mod identity;
mod login;
mod net;
mod share;
mod stream;
mod ticket;
pub mod tunnel;

pub use identity::{Identity, verify_proof};
pub use iroh::endpoint::{Path, PathList};
pub use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};
pub use login::{ADDR_PREFIX, LOGIN_PREFIX, LoginTicket, MAX_ACCOUNT_LEN, decode_addr, encode_addr, valid_account};
pub use net::{
    CTL_ALPN, CtlIncoming, LOGIN_ALPN, LoginIncoming, Net, NetOptions, PathInfo, RelayConfig, RemoteConn, TUNNEL_ALPN, TunnelIncoming, Unreachable,
};
pub use share::{SHARE_ORIGIN, SHARE_PREFIX, ShareTicket};
pub use stream::{AsyncStream, BiStream, Conn, Stream};
pub use ticket::{INVITE_SECRET_LEN, PairTicket, TICKET_PREFIX};
