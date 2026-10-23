//! Tunnels (`silo/tunnel/1`): each bi stream from the owner or a peer
//! holding control at server scope becomes one TCP connection to a
//! server-private address (browser blocks), or one connection to a
//! registered service's unix socket ([`crate::Config::services`]).

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use silo_transport::{
    BiStream, EndpointId, RemoteConn, TunnelIncoming,
    tunnel::{self, TunnelErrCode},
};
use tokio::{
    net::{TcpStream, UnixStream},
    sync::Semaphore,
};

use crate::Server;

/// Concurrent tunnels per peer; extra ones get `busy`.
pub const MAX_STREAMS_PER_PEER: usize = 256;
pub const IDLE: Duration = Duration::from_secs(10 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a service may take to name (and start) its socket.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether `peer` may tunnel, and the grant it tunnels under (`None`:
/// this server's own endpoint). Tunnels need control at server scope.
fn authorized(server: &Server, peer: EndpointId) -> Option<Option<String>> {
    let id = peer.to_string();
    if id == server.info.endpoint_id {
        return Some(None);
    }
    server.auth().tunnel_grant(&id).map(|g| Some(g.id))
}

pub async fn accept_loop(server: Arc<Server>, mut incoming: TunnelIncoming) {
    let mut limits: HashMap<EndpointId, Arc<Semaphore>> = HashMap::new();
    let live: Arc<Mutex<HashSet<u64>>> = Arc::default();
    while let Some((rc, stream)) = incoming.next().await {
        let peer = rc.remote_id();
        let Some(grant) = authorized(&server, peer) else {
            tracing::debug!(%peer, "unauthorized tunnel");
            rc.close(b"unauthorized");
            continue;
        };
        track(&server, &live, &rc, grant.as_deref());
        let limit = limits.entry(peer).or_insert_with(|| Arc::new(Semaphore::new(MAX_STREAMS_PER_PEER)));
        let permit = limit.clone().try_acquire_owned().ok();
        let server = server.clone();
        tokio::spawn(async move {
            let Some(_permit) = permit else {
                let mut stream = stream;
                let _ = tunnel::reply(&mut stream, Err(TunnelErrCode::Busy)).await;
                return;
            };
            handle(&server, stream).await;
        });
    }
}

/// Register the connection with `Auth` so revoking its grant closes it.
fn track(server: &Arc<Server>, live: &Arc<Mutex<HashSet<u64>>>, rc: &RemoteConn, grant: Option<&str>) {
    if !live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(rc.id()) {
        return;
    }
    {
        let mut auth = server.auth();
        auth.add_live(rc.remote_id().to_string(), rc.clone());
        if let Some(grant) = grant {
            auth.bind(rc.id(), grant);
        }
    }
    let (server, live, rc) = (server.clone(), live.clone(), rc.clone());
    tokio::spawn(async move {
        rc.closed().await;
        server.auth().remove_live(rc.id());
        live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&rc.id());
    });
}

async fn handle(server: &Server, mut stream: BiStream) {
    let Ok(Ok(open)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, tunnel::read_open(&mut stream)).await else {
        let _ = tunnel::reply(&mut stream, Err(TunnelErrCode::BadRequest)).await;
        return;
    };
    if let Some(name) = &open.service {
        let unix = match service(server, name).await {
            Ok(unix) => unix,
            Err(code) => {
                tracing::debug!(service = name, ?code, "service tunnel refused");
                let _ = tunnel::reply(&mut stream, Err(code)).await;
                return;
            }
        };
        if tunnel::reply(&mut stream, Ok(())).await.is_ok() {
            // No idle limit: an agent block may sit quiet for hours.
            tunnel::splice(stream, unix, None).await;
        }
        return;
    }
    let tcp = match connect(&open.host, open.port).await {
        Ok(tcp) => tcp,
        Err(code) => {
            tracing::debug!(host = open.host, port = open.port, ?code, "tunnel refused");
            let _ = tunnel::reply(&mut stream, Err(code)).await;
            return;
        }
    };
    if tunnel::reply(&mut stream, Ok(())).await.is_err() {
        return;
    }
    tunnel::splice(stream, tcp, Some(IDLE)).await;
}

/// Ask the service `name` for its socket and connect to it, all within
/// [`SERVICE_TIMEOUT`].
async fn service(server: &Server, name: &str) -> Result<UnixStream, TunnelErrCode> {
    let socket = server.services.get(name).ok_or(TunnelErrCode::NotFound)?.clone();
    let attempt = async {
        let path = socket().await.map_err(|_| TunnelErrCode::Unavailable)?;
        UnixStream::connect(path).await.map_err(|_| TunnelErrCode::Unavailable)
    };
    tokio::time::timeout(SERVICE_TIMEOUT, attempt).await.unwrap_or(Err(TunnelErrCode::Unavailable))
}

/// Resolve `host`, keep only server-private addresses, and connect to the
/// first that answers within [`CONNECT_TIMEOUT`].
async fn connect(host: &str, port: u16) -> Result<TcpStream, TunnelErrCode> {
    if !tunnel::is_private_host(host) {
        return Err(TunnelErrCode::Forbidden);
    }
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let attempt = async {
        let addrs = tokio::net::lookup_host((bare, port)).await.map_err(|_| TunnelErrCode::ConnectFailed)?;
        let addrs: Vec<SocketAddr> = addrs.filter(|a| tunnel::is_private_ip(a.ip())).collect();
        if addrs.is_empty() {
            return Err(TunnelErrCode::Forbidden);
        }
        for addr in addrs {
            if let Ok(tcp) = TcpStream::connect(addr).await {
                return Ok(tcp);
            }
        }
        Err(TunnelErrCode::ConnectFailed)
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt).await.unwrap_or(Err(TunnelErrCode::ConnectFailed))
}
