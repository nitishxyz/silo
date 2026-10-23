//! OAuth login callbacks for a remote otto: while a sign-in on the remote
//! host waits for the browser's redirect to `localhost:<port><path>`, the
//! same loopback port here forwards that one request to the remote's
//! loopback through a [`TunnelDialer`]. Nothing else is forwarded.

use std::{io, sync::Arc, time::Duration};

use silo_transport::tunnel;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};

use crate::tunnel::TunnelDialer;

const MAX_HEAD: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 4;

/// Listening forward; its listeners close when dropped or when its ttl
/// passes (connections already forwarded finish on their own).
pub struct CallbackForward {
    task: JoinHandle<()>,
}

#[derive(Debug)]
pub enum ForwardError {
    /// Something here already listens on the port.
    PortBusy(u16),
    Bind(io::Error),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardError::PortBusy(port) => write!(f, "port {port} is in use"),
            ForwardError::Bind(e) => write!(f, "listen for the sign-in callback: {e}"),
        }
    }
}

impl std::error::Error for ForwardError {}

impl CallbackForward {
    /// Listen on `127.0.0.1:local_port` and `[::1]:local_port` (skipped
    /// where IPv6 is unavailable) for `ttl`, forwarding `GET <path>` and
    /// `GET <path>?…` to the remote's `127.0.0.1:remote_port`; anything
    /// else gets a local `404`. Must be called within a tokio runtime.
    ///
    /// # Errors
    /// [`ForwardError::PortBusy`] when either address is taken, else
    /// [`ForwardError::Bind`].
    pub fn start(dialer: Arc<TunnelDialer>, local_port: u16, remote_port: u16, path: String, ttl: Duration) -> Result<CallbackForward, ForwardError> {
        let busy = |e: io::Error| match e.kind() {
            io::ErrorKind::AddrInUse => ForwardError::PortBusy(local_port),
            _ => ForwardError::Bind(e),
        };
        let mut listeners = vec![listen(([127, 0, 0, 1], local_port).into()).map_err(busy)?];
        match listen((std::net::Ipv6Addr::LOCALHOST, local_port).into()) {
            Ok(v6) => listeners.push(v6),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(ForwardError::PortBusy(local_port)),
            Err(e) => tracing::debug!("callback forward: no IPv6 loopback: {e}"),
        }
        let route = Arc::new(Route { dialer, remote_port, path, limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)) });
        let task = tokio::spawn(async move {
            let mut accepting = JoinSet::new();
            for listener in listeners {
                accepting.spawn(accept_loop(listener, route.clone()));
            }
            tokio::time::sleep(ttl).await;
            // Dropping the set aborts the loops and closes the listeners.
        });
        Ok(CallbackForward { task })
    }
}

impl Drop for CallbackForward {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn listen(addr: std::net::SocketAddr) -> io::Result<TcpListener> {
    let std = std::net::TcpListener::bind(addr)?;
    std.set_nonblocking(true)?;
    TcpListener::from_std(std)
}

struct Route {
    dialer: Arc<TunnelDialer>,
    remote_port: u16,
    path: String,
    limit: Arc<Semaphore>,
}

async fn accept_loop(listener: TcpListener, route: Arc<Route>) {
    loop {
        let Ok(permit) = route.limit.clone().acquire_owned().await else { return };
        let Ok((tcp, _)) = listener.accept().await else { return };
        let route = route.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = forward(&route, tcp).await {
                tracing::debug!("callback forward: {error}");
            }
        });
    }
}

async fn forward(route: &Route, mut tcp: TcpStream) -> io::Result<()> {
    let buf = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut tcp)).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    if !allows(&route.path, &buf) {
        return reply(&mut tcp, "404 Not Found").await;
    }
    let mut upstream = match route.dialer.open("127.0.0.1", route.remote_port).await {
        Ok(upstream) => upstream,
        Err(e) => {
            reply(&mut tcp, "502 Bad Gateway").await?;
            return Err(e);
        }
    };
    upstream.write_all(&buf).await?;
    tunnel::splice(tcp, upstream, Some(IDLE)).await;
    Ok(())
}

/// Whether the request line is `GET <path>` or `GET <path>?…`.
fn allows(path: &str, buf: &[u8]) -> bool {
    let line = buf.split(|&b| b == b'\n').next().unwrap_or_default();
    let Ok(line) = std::str::from_utf8(line) else { return false };
    let mut parts = line.trim_end_matches('\r').split(' ');
    let (Some("GET"), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let Some(after) = target.strip_prefix(path) else { return false };
    version.starts_with("HTTP/") && parts.next().is_none() && (after.is_empty() || after.starts_with('?'))
}

async fn reply(tcp: &mut TcpStream, status: &str) -> io::Result<()> {
    tcp.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await
}

/// Everything read until the end of the request head (which may include
/// the start of a body).
async fn read_head(tcp: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(2048);
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() >= MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        let mut chunk = [0u8; 4096];
        let n = tcp.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof before request head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use silo_transport::{EndpointAddr, Net};

    use super::*;

    /// A dialer whose server is never reached (nothing here is forwarded).
    async fn idle_dialer(name: &str) -> Arc<TunnelDialer> {
        let dir = std::path::PathBuf::from(format!("/tmp/silo-client-callback-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = crate::client_identity(&dir).unwrap();
        let net = Net::bind(&id, silo_transport::RelayConfig::Disabled, false).await.unwrap();
        let server = EndpointAddr::new(silo_transport::Identity::load_or_create(dir.join("srv")).unwrap().id());
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(TunnelDialer::new(Arc::new(net), server))
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
    }

    async fn port_frees(port: u16) -> bool {
        for _ in 0..200 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[test]
    fn only_get_of_the_path_is_allowed() {
        for ok in ["GET /auth/callback HTTP/1.1\r\n\r\n", "GET /auth/callback?code=x&state=y HTTP/1.1\r\nHost: localhost\r\n\r\n"] {
            assert!(allows("/auth/callback", ok.as_bytes()), "{ok:?}");
        }
        for bad in [
            "POST /auth/callback HTTP/1.1\r\n\r\n",
            "GET /auth/callbackx HTTP/1.1\r\n\r\n",
            "GET /auth/callback/../x HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "GET http://evil/auth/callback HTTP/1.1\r\n\r\n",
            "GET /auth/callback\r\n\r\n",
        ] {
            assert!(!allows("/auth/callback", bad.as_bytes()), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn busy_port_is_reported() {
        let dialer = idle_dialer("busy").await;
        let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        let err = CallbackForward::start(dialer.clone(), port, port, "/cb".into(), Duration::from_secs(60)).err().expect("bound a busy port");
        assert!(matches!(err, ForwardError::PortBusy(p) if p == port), "{err}");
        drop(taken);
        // IPv6 loopback taken (where there is one) is busy too.
        if let Ok(taken6) = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)) {
            let port = taken6.local_addr().unwrap().port();
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                let err = CallbackForward::start(dialer, port, port, "/cb".into(), Duration::from_secs(60)).err().expect("bound a busy v6 port");
                assert!(matches!(err, ForwardError::PortBusy(p) if p == port), "{err}");
            }
        }
    }

    #[tokio::test]
    async fn dropped_forward_frees_the_port() {
        let dialer = idle_dialer("drop").await;
        let port = free_port();
        let forward = CallbackForward::start(dialer.clone(), port, port, "/cb".into(), Duration::from_secs(60)).unwrap();
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err(), "not listening");
        drop(forward);
        assert!(port_frees(port).await, "port still bound after drop");

        let port = free_port();
        let _forward = CallbackForward::start(dialer, port, port, "/cb".into(), Duration::from_millis(100)).unwrap();
        assert!(port_frees(port).await, "port still bound after the ttl");
    }
}
