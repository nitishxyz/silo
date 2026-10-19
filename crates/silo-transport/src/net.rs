use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::transports::{PathSelection, PathSelectionContext, PathSelector};
use iroh::endpoint::{Connection, ConnectionError, PathList, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::sync::mpsc;

use crate::identity::Identity;
use crate::stream::BiStream;

/// Control-plane ALPN: framed JSON-RPC + per-block data streams.
pub const CTL_ALPN: &[u8] = b"silo/ctl/1";
/// Browser-block tunnels: one TCP connection per bi stream (see `tunnel`).
pub const TUNNEL_ALPN: &[u8] = b"silo/tunnel/1";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelayConfig {
    /// n0 production relays plus n0 DNS/pkarr address lookup.
    #[default]
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

/// Endpoint options. `Default` = n0 relays, mDNS off, normal path selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetOptions {
    pub relay: RelayConfig,
    pub mdns: bool,
    /// Keep every connection on the path it was established on (the dialed
    /// address for clients, the peer's source address for servers). iroh
    /// still learns holepunch candidates but closes those paths instead of
    /// switching to them. For tests that route traffic through a proxy.
    pub pin_initial_path: bool,
    /// Fixed UDP port for the IPv4 and IPv6 sockets (unspecified address),
    /// so direct addresses stay valid across restarts. `None`: random.
    pub port: Option<u16>,
}

impl Net {
    pub async fn bind(identity: &Identity, relay: RelayConfig, mdns: bool) -> Result<Net> {
        let opts = NetOptions {
            relay,
            mdns,
            pin_initial_path: false,
            port: None,
        };
        Net::bind_with(identity, opts).await
    }

    pub async fn bind_with(identity: &Identity, opts: NetOptions) -> Result<Net> {
        let NetOptions {
            relay,
            mdns,
            pin_initial_path,
            port,
        } = opts;
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
        if pin_initial_path {
            builder = builder.path_selector(Arc::new(PinInitialPath));
        }
        if let Some(port) = port {
            builder = builder
                .bind_addr((std::net::Ipv4Addr::UNSPECIFIED, port))
                .context("bind address")?
                .bind_addr((std::net::Ipv6Addr::UNSPECIFIED, port))
                .context("bind address")?;
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
        self.connect(addr.into(), CTL_ALPN).await
    }

    /// Connect with TUNNEL_ALPN; every bi stream is one tunnel.
    pub async fn dial_tunnel(&self, addr: impl Into<EndpointAddr>) -> Result<RemoteConn> {
        self.connect(addr.into(), TUNNEL_ALPN).await
    }

    async fn connect(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<RemoteConn> {
        let id = addr.id;
        let conn = self
            .endpoint
            .connect(addr, alpn)
            .await
            .with_context(|| format!("connect to {id}"))?;
        Ok(RemoteConn { conn })
    }

    /// Start accepting CTL_ALPN and TUNNEL_ALPN connections. Each CTL
    /// connection is delivered with its first bi stream once the peer has
    /// written on it; every TUNNEL bi stream is delivered on its own.
    pub fn serve(&mut self, backlog: usize) -> Result<(CtlIncoming, TunnelIncoming)> {
        if self.router.is_some() {
            bail!("serve called twice");
        }
        let (tx, rx) = mpsc::channel(backlog.max(1));
        let (tunnel_tx, tunnel_rx) = mpsc::channel(backlog.max(1));
        let router = Router::builder(self.endpoint.clone())
            .accept(CTL_ALPN, CtlHandler { tx })
            .accept(TUNNEL_ALPN, TunnelHandler { tx: tunnel_tx })
            .spawn();
        self.router = Some(router);
        Ok((CtlIncoming { rx }, TunnelIncoming { rx: tunnel_rx }))
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

    /// Currently open network paths of this connection.
    pub fn paths(&self) -> PathList<'_> {
        self.conn.paths()
    }

    /// Round-trip time of the selected path, if one is selected.
    pub fn rtt(&self) -> Option<Duration> {
        self.conn.paths().iter().find(|p| p.is_selected()).map(|p| p.rtt())
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

/// Stream of TUNNEL_ALPN bi streams with their connection.
#[derive(Debug)]
pub struct TunnelIncoming {
    rx: mpsc::Receiver<(RemoteConn, BiStream)>,
}

impl TunnelIncoming {
    /// `None` once the endpoint is shut down.
    pub async fn next(&mut self) -> Option<(RemoteConn, BiStream)> {
        self.rx.recv().await
    }
}

#[derive(Debug)]
struct TunnelHandler {
    tx: mpsc::Sender<(RemoteConn, BiStream)>,
}

impl ProtocolHandler for TunnelHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let rc = RemoteConn { conn };
        while let Ok(stream) = rc.accept_stream().await {
            if self.tx.send((rc.clone(), stream)).await.is_err() {
                rc.close(b"server not accepting");
                break;
            }
        }
        Ok(())
    }
}

/// Never moves off the first selected path.
#[derive(Debug)]
struct PinInitialPath;

impl PathSelector for PinInitialPath {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let mut sel = PathSelection::none();
        if ctx.current().is_none()
            && let Some(first) = ctx.paths().next()
        {
            sel.set(&first);
        }
        sel
    }
}
