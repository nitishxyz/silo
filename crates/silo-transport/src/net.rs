use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, ConnectionError, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::sync::mpsc;

use crate::identity::Identity;
use crate::stream::BiStream;

/// Control-plane ALPN: framed JSON-RPC + per-block data streams.
pub const CTL_ALPN: &[u8] = b"silo/ctl/1";
/// Reserved for browser-block tunnels (Phase 6). Not served yet.
pub const TUNNEL_ALPN: &[u8] = b"silo/tunnel/1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayConfig {
    /// n0 production relays plus n0 DNS/pkarr address lookup.
    Default,
    /// A single self-hosted relay; no n0 services.
    Custom(RelayUrl),
    /// No relays and no n0 services: direct addresses / mDNS only.
    Disabled,
}

/// One iroh endpoint per process.
#[derive(Debug)]
pub struct Net {
    endpoint: Endpoint,
    router: Option<Router>,
}

impl Net {
    pub async fn bind(identity: &Identity, relay: RelayConfig, mdns: bool) -> Result<Net> {
        let builder = match relay {
            RelayConfig::Default => Endpoint::builder(presets::N0),
            RelayConfig::Custom(url) => {
                Endpoint::builder(presets::Minimal).relay_mode(RelayMode::custom([url]))
            }
            RelayConfig::Disabled => {
                Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
            }
        };
        let mut builder = builder.secret_key(identity.secret().clone());
        if mdns {
            builder = builder.address_lookup(MdnsAddressLookup::builder());
        }
        let endpoint = builder.bind().await.context("bind iroh endpoint")?;
        Ok(Net {
            endpoint,
            router: None,
        })
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Currently known address (id + home relay + direct addrs).
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Wait until a home relay (if any) is connected and addresses are known.
    pub async fn online(&self) {
        self.endpoint.online().await
    }

    /// Local UDP sockets (may be unspecified addresses such as 0.0.0.0).
    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    pub async fn dial(&self, addr: impl Into<EndpointAddr>) -> Result<RemoteConn> {
        let addr = addr.into();
        let id = addr.id;
        let conn = self
            .endpoint
            .connect(addr, CTL_ALPN)
            .await
            .with_context(|| format!("connect to {id}"))?;
        Ok(RemoteConn { conn })
    }

    /// Start accepting CTL_ALPN connections. Each connection is delivered
    /// with its first bi stream once the peer has written on it.
    pub fn serve_ctl(&mut self, backlog: usize) -> Result<CtlIncoming> {
        if self.router.is_some() {
            bail!("serve_ctl called twice");
        }
        let (tx, rx) = mpsc::channel(backlog.max(1));
        let router = Router::builder(self.endpoint.clone())
            .accept(CTL_ALPN, CtlHandler { tx })
            .spawn();
        self.router = Some(router);
        Ok(CtlIncoming { rx })
    }

    /// Close all connections and the endpoint, waiting for a clean shutdown.
    pub async fn close(self) -> Result<()> {
        match self.router {
            Some(router) => router.shutdown().await.context("router shutdown")?,
            None => self.endpoint.close().await,
        }
        Ok(())
    }
}

/// An established CTL_ALPN connection. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RemoteConn {
    conn: Connection,
}

impl RemoteConn {
    /// Open a new bi stream. The peer only sees it after the first write.
    pub async fn open_stream(&self) -> Result<BiStream> {
        let (send, recv) = self.conn.open_bi().await.context("open bi stream")?;
        Ok(BiStream::new(send, recv))
    }

    /// Accept the next bi stream opened by the peer.
    pub async fn accept_stream(&self) -> Result<BiStream> {
        let (send, recv) = self.conn.accept_bi().await.context("accept bi stream")?;
        Ok(BiStream::new(send, recv))
    }

    pub fn remote_id(&self) -> EndpointId {
        self.conn.remote_id()
    }

    /// Identifier unique among this process's live connections.
    pub fn id(&self) -> u64 {
        self.conn.stable_id() as u64
    }

    /// Resolves when the connection is closed, with the reason.
    pub async fn closed(&self) -> ConnectionError {
        self.conn.closed().await
    }

    pub fn close(&self, reason: &[u8]) {
        self.conn.close(0u32.into(), reason);
    }
}

/// Stream of accepted CTL_ALPN connections with their first bi stream.
#[derive(Debug)]
pub struct CtlIncoming {
    rx: mpsc::Receiver<(RemoteConn, BiStream)>,
}

impl CtlIncoming {
    /// `None` once the endpoint is shut down.
    pub async fn next(&mut self) -> Option<(RemoteConn, BiStream)> {
        self.rx.recv().await
    }
}

#[derive(Debug)]
struct CtlHandler {
    tx: mpsc::Sender<(RemoteConn, BiStream)>,
}

impl ProtocolHandler for CtlHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let rc = RemoteConn { conn };
        let first = match rc.accept_stream().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(remote = %rc.remote_id(), "no first ctl stream: {e:#}");
                return Ok(());
            }
        };
        if self.tx.send((rc.clone(), first)).await.is_err() {
            rc.close(b"server not accepting");
        }
        Ok(())
    }
}
