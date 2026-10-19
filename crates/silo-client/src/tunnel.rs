//! Browser-block tunnels, client side: one loopback HTTP proxy per remote
//! server, used as the fixed proxy of that server's browser profiles.
//! Server-private hosts go through one `silo/tunnel/1` stream per proxied
//! connection on a cached TUNNEL_ALPN connection; every other host is
//! dialed directly from the client.

use std::{
    io,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use data_encoding::{BASE64, HEXLOWER};
use silo_transport::{
    BiStream, EndpointAddr, Net, RemoteConn,
    tunnel::{self, TunnelErr, TunnelErrCode},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task::JoinHandle,
};

/// Idle tunnels are closed after this long (the server does the same).
pub const IDLE: Duration = Duration::from_secs(10 * 60);
const MAX_HEAD: usize = 64 * 1024;
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Proxy-auth user name; the password is a per-proxy random token.
pub const PROXY_USER: &str = "silo";

/// A running proxy; stops when dropped.
pub struct TunnelProxy {
    port: u16,
    password: String,
    task: JoinHandle<()>,
}

/// Which hosts are tunneled, and which resolved addresses a direct
/// connection may use. Injected so tests can treat loopback as public.
#[derive(Clone, Copy)]
struct Policy {
    tunneled: fn(&str) -> bool,
    direct_ok: fn(IpAddr) -> bool,
}

const POLICY: Policy = Policy {
    tunneled: tunnel::is_private_host,
    direct_ok: |ip| !tunnel::is_non_public_ip(ip),
};

impl TunnelProxy {
    /// Listen on `127.0.0.1:0`; tunnel server-private hosts to the server at
    /// `addr` and connect to public ones directly. Every request must carry
    /// [`Self::credentials`] as `Proxy-Authorization: Basic`.
    ///
    /// # Errors
    /// The listener could not be bound.
    pub async fn start<N>(net: N, addr: EndpointAddr) -> anyhow::Result<TunnelProxy>
    where
        N: Deref<Target = Net> + Send + Sync + 'static,
    {
        Self::start_with(net, addr, POLICY).await
    }

    async fn start_with<N>(
        net: N,
        addr: EndpointAddr,
        policy: Policy,
    ) -> anyhow::Result<TunnelProxy>
    where
        N: Deref<Target = Net> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind tunnel proxy")?;
        let port = listener.local_addr()?.port();
        let password = HEXLOWER.encode(&rand::random::<[u8; 32]>());
        let expected = BASE64.encode(format!("{PROXY_USER}:{password}").as_bytes());
        let dialer = Arc::new(Dialer {
            net,
            addr,
            conn: Mutex::new(None),
            policy,
            expected,
        });
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let dialer = dialer.clone();
                tokio::spawn(async move {
                    if let Err(error) = proxy(&*dialer, tcp).await {
                        tracing::debug!("tunnel proxy: {error:#}");
                    }
                });
            }
        });
        Ok(TunnelProxy {
            port,
            password,
            task,
        })
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// `(user, password)` for HTTP Basic proxy auth (realm `silo`).
    #[must_use]
    pub fn credentials(&self) -> (&str, &str) {
        (PROXY_USER, &self.password)
    }
}

impl Drop for TunnelProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Dialer<N> {
    net: N,
    addr: EndpointAddr,
    conn: Mutex<Option<RemoteConn>>,
    policy: Policy,
    /// Base64 of `user:password`, as sent after `Basic `.
    expected: String,
}

impl<N: Deref<Target = Net>> Dialer<N> {
    /// A handshaken tunnel to `host:port` on the cached connection. Any
    /// transport failure drops the cache so the next tunnel redials.
    async fn open(&self, host: &str, port: u16) -> io::Result<BiStream> {
        let rc = {
            let mut cached = self.conn.lock().await;
            match &*cached {
                Some(rc) => rc.clone(),
                None => {
                    let rc = self
                        .net
                        .dial_tunnel(self.addr.clone())
                        .await
                        .map_err(io::Error::other)?;
                    cached.insert(rc).clone()
                }
            }
        };
        let result = async {
            let mut stream = rc.open_stream().await.map_err(io::Error::other)?;
            tunnel::open(&mut stream, host, port).await?;
            Ok(stream)
        }
        .await;
        if let Err(e) = &result
            && refusal(e).is_none()
        {
            let mut cached = self.conn.lock().await;
            if cached.as_ref().is_some_and(|c| c.id() == rc.id()) {
                *cached = None;
            }
        }
        result
    }
}

fn refusal(e: &io::Error) -> Option<TunnelErrCode> {
    e.get_ref()
        .and_then(|e| e.downcast_ref::<TunnelErr>())
        .map(|e| e.code)
}

fn status_for(e: &io::Error) -> &'static str {
    match refusal(e) {
        Some(TunnelErrCode::Forbidden | TunnelErrCode::Unauthorized) => "403 Forbidden",
        Some(TunnelErrCode::Busy) => "503 Service Unavailable",
        _ => "502 Bad Gateway",
    }
}

async fn proxy<N: Deref<Target = Net>>(
    dialer: &Dialer<N>,
    mut tcp: TcpStream,
) -> anyhow::Result<()> {
    let (head, rest) = read_head(&mut tcp).await?;
    if !authorized(&head, &dialer.expected) {
        let reply = "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"silo\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        tcp.write_all(reply.as_bytes()).await?;
        anyhow::bail!("missing or wrong proxy credentials");
    }
    let req = Request::parse(&head).context("bad proxy request")?;
    if !(dialer.policy.tunneled)(&req.host) {
        let connect = direct(&req.host, req.port, dialer.policy.direct_ok);
        let direct = match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, connect).await {
            Ok(Ok(direct)) => direct,
            Ok(Err((status, e))) => return refuse(tcp, status, e).await,
            Err(_) => {
                let e = anyhow::anyhow!("connect timed out");
                return refuse(tcp, "504 Gateway Timeout", e).await;
            }
        };
        return relay(tcp, direct, &req, &rest).await;
    }
    let stream = match dialer.open(&req.host, req.port).await {
        Ok(stream) => stream,
        Err(e) => return refuse(tcp, status_for(&e), e.into()).await,
    };
    relay(tcp, stream, &req, &rest).await
}

/// Resolve once, refuse if any address is not public (a public name must
/// not reach this machine's or its LAN's services), then connect only to
/// the vetted addresses so a second lookup can't change the answer.
async fn direct(
    host: &str,
    port: u16,
    ok: fn(IpAddr) -> bool,
) -> Result<TcpStream, (&'static str, anyhow::Error)> {
    let bad_gateway = |e: anyhow::Error| ("502 Bad Gateway", e);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| bad_gateway(e.into()))?
        .collect();
    if addrs.is_empty() {
        return Err(bad_gateway(anyhow::anyhow!("{host} has no addresses")));
    }
    if let Some(bad) = addrs.iter().find(|a| !ok(a.ip())) {
        let e = anyhow::anyhow!("{host} resolves to non-public {}", bad.ip());
        return Err(("403 Forbidden", e));
    }
    let mut last = anyhow::anyhow!("connect {host}");
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(tcp) => return Ok(tcp),
            Err(e) => last = e.into(),
        }
    }
    Err(bad_gateway(last))
}

/// Whether `head` carries `Proxy-Authorization: Basic <expected>`,
/// compared in constant time.
fn authorized(head: &str, expected: &str) -> bool {
    let mut ok = false;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("proxy-authorization") {
            continue;
        }
        let mut parts = value.trim().splitn(2, ' ');
        let scheme = parts.next().unwrap_or_default();
        let token = parts.next().unwrap_or_default().trim();
        ok |= scheme.eq_ignore_ascii_case("basic") && ct_eq(token.as_bytes(), expected.as_bytes());
    }
    ok
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn refuse(mut tcp: TcpStream, status: &str, error: anyhow::Error) -> anyhow::Result<()> {
    let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    tcp.write_all(reply.as_bytes()).await?;
    Err(error)
}

/// Answer CONNECT (or forward the origin-form head), then splice.
async fn relay<S>(
    mut tcp: TcpStream,
    mut upstream: S,
    req: &Request,
    rest: &[u8],
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match &req.forward {
        None => {
            tcp.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?
        }
        Some(head) => upstream.write_all(head.as_bytes()).await?,
    }
    upstream.write_all(rest).await?;
    tunnel::splice(tcp, upstream, IDLE).await;
    Ok(())
}

/// The request head (through the blank line) and any bytes read past it.
async fn read_head(tcp: &mut TcpStream) -> anyhow::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((
                String::from_utf8(buf).context("non-utf8 request head")?,
                rest,
            ));
        }
        anyhow::ensure!(buf.len() < MAX_HEAD, "request head too large");
        let mut chunk = [0u8; 4096];
        let n = tcp.read(&mut chunk).await?;
        anyhow::ensure!(n > 0, "eof before request head");
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    host: String,
    port: u16,
    /// Absolute-form requests: the head to send upstream (origin-form).
    /// `None` for CONNECT.
    forward: Option<String>,
}

impl Request {
    fn parse(head: &str) -> Option<Request> {
        let (line, headers) = head.split_once("\r\n")?;
        let mut parts = line.split(' ');
        let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
        if method.eq_ignore_ascii_case("CONNECT") {
            let (host, port) = split_host_port(target, None)?;
            return Some(Request {
                host,
                port,
                forward: None,
            });
        }
        let rest = target
            .strip_prefix("http://")
            .or_else(|| target.strip_prefix("ws://"))?;
        let (authority, path) = rest
            .find('/')
            .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        let (host, port) = split_host_port(authority, Some(80))?;
        let upgrade = headers.lines().any(|h| {
            h.split_once(':')
                .is_some_and(|(n, _)| n.trim().eq_ignore_ascii_case("upgrade"))
        });
        let mut forward = format!("{method} {path} {version}\r\n");
        for h in headers.lines().filter(|h| !h.is_empty()) {
            let name = h.split_once(':').map_or(h, |(n, _)| n).trim();
            let hop = name.eq_ignore_ascii_case("proxy-connection")
                || name.eq_ignore_ascii_case("proxy-authorization");
            // One upstream per client connection: only the first head is
            // parsed; everything after it is spliced to that same upstream,
            // which is told `Connection: close`. So `Content-Length` /
            // `Transfer-Encoding` pass through untouched: a desync between
            // client and upstream can only target the upstream the client
            // already addressed, never another host or the tunnel policy.
            let keep_alive = !upgrade && name.eq_ignore_ascii_case("connection");
            if !hop && !keep_alive {
                forward.push_str(h);
                forward.push_str("\r\n");
            }
        }
        if !upgrade {
            forward.push_str("Connection: close\r\n");
        }
        forward.push_str("\r\n");
        Some(Request {
            host,
            port,
            forward: Some(forward),
        })
    }
}

/// `host:port`, `[v6]:port`, or (with a default) a bare host. IPv6 hosts are
/// returned without brackets.
fn split_host_port(authority: &str, default: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => default?,
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replies with the request head it received, then closes.
    async fn head_echo_server() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (head, _) = read_head(&mut tcp).await.unwrap();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{head}",
                        head.len()
                    );
                    tcp.write_all(reply.as_bytes()).await.unwrap();
                });
            }
        });
        port
    }

    async fn roundtrip(port: u16, send: &[u8]) -> String {
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tcp.write_all(send).await.unwrap();
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(10), tcp.read_to_string(&mut out))
            .await
            .unwrap()
            .unwrap();
        out
    }

    /// A proxy whose (unreachable) server is never dialed: nothing is
    /// tunneled. `direct_ok` decides which resolved addresses are public.
    async fn direct_proxy(name: &str, direct_ok: fn(IpAddr) -> bool) -> (TunnelProxy, String) {
        let dir = std::path::PathBuf::from(format!(
            "/tmp/silo-client-tunnel-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let id = crate::client_identity(&dir).unwrap();
        let net = Net::bind(&id, silo_transport::RelayConfig::Disabled, false)
            .await
            .unwrap();
        let server = EndpointAddr::new(
            silo_transport::Identity::load_or_create(dir.join("srv"))
                .unwrap()
                .id(),
        );
        let policy = Policy {
            tunneled: |_| false,
            direct_ok,
        };
        let proxy = TunnelProxy::start_with(Arc::new(net), server, policy)
            .await
            .unwrap();
        let (user, pass) = proxy.credentials();
        let auth = format!(
            "Proxy-Authorization: Basic {}\r\n",
            BASE64.encode(format!("{user}:{pass}").as_bytes())
        );
        let _ = std::fs::remove_dir_all(&dir);
        (proxy, auth)
    }

    #[tokio::test]
    async fn public_hosts_go_direct() {
        let (proxy, auth) = direct_proxy("direct", |_| true).await;
        let web = head_echo_server().await;

        let get = format!(
            "GET http://127.0.0.1:{web}/p?q HTTP/1.1\r\nHost: x\r\n{auth}Proxy-Connection: keep-alive\r\n\r\n"
        );
        let out = roundtrip(proxy.port(), get.as_bytes()).await;
        assert!(
            out.ends_with("\r\n\r\nGET /p?q HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
            "{out}"
        );

        let connect =
            format!("CONNECT 127.0.0.1:{web} HTTP/1.1\r\n{auth}\r\nGET /raw HTTP/1.1\r\n\r\n");
        let out = roundtrip(proxy.port(), connect.as_bytes()).await;
        assert!(
            out.starts_with("HTTP/1.1 200 Connection Established\r\n\r\nHTTP/1.1 200 OK"),
            "{out}"
        );
        assert!(out.ends_with("GET /raw HTTP/1.1\r\n\r\n"), "{out}");
    }

    #[tokio::test]
    async fn requires_proxy_auth() {
        let (proxy, auth) = direct_proxy("auth", |_| true).await;
        let web = head_echo_server().await;
        let wrong = format!(
            "Proxy-Authorization: Basic {}\r\n",
            BASE64.encode(b"silo:wrong")
        );
        for creds in ["", wrong.as_str(), "Proxy-Authorization: Bearer x\r\n"] {
            for req in [
                format!("GET http://127.0.0.1:{web}/ HTTP/1.1\r\n{creds}\r\n"),
                format!("CONNECT 127.0.0.1:{web} HTTP/1.1\r\n{creds}\r\n"),
            ] {
                let out = roundtrip(proxy.port(), req.as_bytes()).await;
                assert!(out.starts_with("HTTP/1.1 407 "), "{req:?}: {out}");
                assert!(
                    out.contains("\r\nProxy-Authenticate: Basic realm=\"silo\"\r\n"),
                    "{out}"
                );
            }
        }
        let ok = roundtrip(
            proxy.port(),
            format!("GET http://127.0.0.1:{web}/ HTTP/1.1\r\n{auth}\r\n").as_bytes(),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
        assert!(
            !ok.contains("Proxy-Authorization"),
            "credentials leaked upstream: {ok}"
        );
        assert_ne!(
            proxy.credentials().1,
            direct_proxy("auth2", |_| true).await.0.credentials().1
        );
    }

    #[tokio::test]
    async fn public_name_resolving_to_loopback_is_refused() {
        // Real address policy; `localhost` resolves to loopback via the
        // hosts file, standing in for e.g. `localtest.me` (no network).
        let (proxy, auth) = direct_proxy("rebind", POLICY.direct_ok).await;
        let web = head_echo_server().await;
        for req in [
            format!("GET http://localhost:{web}/ HTTP/1.1\r\n{auth}\r\n"),
            format!("CONNECT localhost:{web} HTTP/1.1\r\n{auth}\r\n"),
            format!("CONNECT 169.254.169.254:80 HTTP/1.1\r\n{auth}\r\n"),
        ] {
            let out = roundtrip(proxy.port(), req.as_bytes()).await;
            assert!(out.starts_with("HTTP/1.1 403 "), "{req:?}: {out}");
        }
    }

    /// Only the first head is parsed: later pipelined requests (even for
    /// another host) go to the first upstream, which was told to close.
    #[tokio::test]
    async fn one_upstream_per_connection() {
        let (proxy, auth) = direct_proxy("pipeline", |_| true).await;
        let a = head_echo_server().await;
        let b = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let bport = b.local_addr().unwrap().port();
        let req = format!(
            "GET http://127.0.0.1:{a}/1 HTTP/1.1\r\n{auth}Content-Length: 0\r\nConnection: keep-alive\r\n\r\nGET http://127.0.0.1:{bport}/2 HTTP/1.1\r\n{auth}\r\n"
        );
        let out = roundtrip(proxy.port(), req.as_bytes()).await;
        assert!(
            out.ends_with("GET /1 HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "{out}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), b.accept())
                .await
                .is_err(),
            "second host was dialed"
        );
    }

    #[test]
    fn parses_connect_and_absolute_form() {
        let connect =
            Request::parse("CONNECT localhost:3000 HTTP/1.1\r\nHost: localhost:3000\r\n\r\n")
                .unwrap();
        assert_eq!(
            connect,
            Request {
                host: "localhost".into(),
                port: 3000,
                forward: None
            }
        );
        let v6 = Request::parse("CONNECT [::1]:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 443));

        let get = Request::parse("GET http://localhost:3000/a?b HTTP/1.1\r\nHost: localhost:3000\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\n\r\n").unwrap();
        assert_eq!((get.host.as_str(), get.port), ("localhost", 3000));
        assert_eq!(
            get.forward.unwrap(),
            "GET /a?b HTTP/1.1\r\nHost: localhost:3000\r\nConnection: close\r\n\r\n"
        );

        let ws = Request::parse(
            "GET http://10.0.0.2/ws HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .unwrap();
        assert_eq!(ws.port, 80);
        assert_eq!(
            ws.forward.unwrap(),
            "GET /ws HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
        );

        assert!(Request::parse("GET /origin-form HTTP/1.1\r\n\r\n").is_none());
    }
}
