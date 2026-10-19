//! `silo/tunnel/1` wire format and destination policy.
//!
//! Every TUNNEL_ALPN bi stream is one TCP connection. The client writes one
//! frame `u32 len (LE, kind + payload) | u8 kind | JSON payload` with
//! [`kind::OPEN`] / [`TunnelOpen`]; the server answers [`kind::OK`] (empty
//! JSON object) or [`kind::ERR`] / [`TunnelErr`]. After `OK` the stream
//! carries raw bytes both ways.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Handshake frames are tiny; anything larger is a protocol error.
const MAX_HANDSHAKE: usize = 4096;

pub mod kind {
    pub const OPEN: u8 = 0x01;
    pub const OK: u8 = 0x02;
    pub const ERR: u8 = 0x03;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelOpen {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelErrCode {
    /// The destination is not server-private.
    Forbidden,
    /// The peer is neither the owner nor a paired device.
    Unauthorized,
    /// DNS, refusal or the 5 s connect timeout.
    ConnectFailed,
    /// Too many concurrent tunnels for this peer.
    Busy,
    /// Malformed handshake.
    BadRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelErr {
    pub code: TunnelErrCode,
}

impl std::fmt::Display for TunnelErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code).map_err(|_| std::fmt::Error)?;
        write!(f, "tunnel refused: {}", code.as_str().unwrap_or("?"))
    }
}

impl std::error::Error for TunnelErr {}

async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, value: &impl Serialize) -> io::Result<()> {
    let payload = serde_json::to_vec(value)?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&payload);
    w.write_all(&out).await?;
    w.flush().await
}

async fn read_raw<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_HANDSHAKE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad tunnel frame length {len}")));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    let payload = buf.split_off(1);
    Ok((buf[0], payload))
}

fn invalid(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Client side: request `host:port` and wait for the verdict. A
/// [`TunnelErr`] is returned inside the `io::Error` (`get_ref`).
pub async fn open<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, host: &str, port: u16) -> io::Result<()> {
    write_json(s, kind::OPEN, &TunnelOpen { host: host.to_owned(), port }).await?;
    match read_raw(s).await? {
        (kind::OK, _) => Ok(()),
        (kind::ERR, payload) => {
            let err: TunnelErr = serde_json::from_slice(&payload).map_err(invalid)?;
            Err(io::Error::new(io::ErrorKind::PermissionDenied, err))
        }
        (other, _) => Err(invalid(format!("unexpected tunnel frame kind {other:#x}"))),
    }
}

/// Server side: read the client's [`TunnelOpen`].
pub async fn read_open<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<TunnelOpen> {
    match read_raw(r).await? {
        (kind::OPEN, payload) => serde_json::from_slice(&payload).map_err(invalid),
        (other, _) => Err(invalid(format!("expected tunnel open, got kind {other:#x}"))),
    }
}

/// Server side: answer the handshake.
pub async fn reply<W: AsyncWrite + Unpin>(w: &mut W, verdict: Result<(), TunnelErrCode>) -> io::Result<()> {
    match verdict {
        Ok(()) => write_json(w, kind::OK, &serde_json::json!({})).await,
        Err(code) => write_json(w, kind::ERR, &TunnelErr { code }).await,
    }
}

/// Copy bytes both ways until both sides reach EOF (each EOF is forwarded
/// as a write shutdown), an error, or `idle` passes with no data.
pub async fn splice<A, B>(a: A, b: B, idle: Duration)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let (mut abuf, mut bbuf) = (vec![0u8; 16 * 1024], vec![0u8; 16 * 1024]);
    let (mut a_open, mut b_open) = (true, true);
    while a_open || b_open {
        let step = tokio::time::timeout(idle, async {
            tokio::select! {
                r = ar.read(&mut abuf), if a_open => (true, r),
                r = br.read(&mut bbuf), if b_open => (false, r),
            }
        });
        let Ok((from_a, read)) = step.await else {
            return;
        };
        let done = if from_a { forward(read, &abuf, &mut a_open, &mut bw).await } else { forward(read, &bbuf, &mut b_open, &mut aw).await };
        if done.is_err() {
            return;
        }
    }
}

async fn forward<W: AsyncWrite + Unpin>(read: io::Result<usize>, buf: &[u8], open: &mut bool, w: &mut W) -> io::Result<()> {
    match read? {
        0 => {
            *open = false;
            w.shutdown().await
        }
        n => w.write_all(&buf[..n]).await,
    }
}

/// Server-private by name alone: `localhost`, `*.localhost` and private,
/// loopback or ULA IP literals (IPv6 optionally bracketed).
pub fn is_private_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    let literal = h.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(&h);
    literal.parse::<IpAddr>().is_ok_and(is_private_ip)
}

/// Tunnel destinations: loopback, RFC 1918 and ULA addresses. Link-local
/// (169.254/16, fe80::/10) is excluded: it holds cloud metadata services.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match unmap(ip) {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Anything a public name must never resolve to for a direct connection:
/// [`is_private_ip`], link-local and unspecified (which reaches loopback).
pub fn is_non_public_ip(ip: IpAddr) -> bool {
    let ip = unmap(ip);
    is_private_ip(ip)
        || ip.is_unspecified()
        || match ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        }
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts() {
        for h in [
            "localhost",
            "app.localhost",
            "LOCALHOST.",
            "127.0.0.1",
            "127.9.9.9",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "::1",
            "[::1]",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_private_host(h), "{h}");
        }
        for h in [
            "example.com",
            "localhost.example.com",
            "8.8.8.8",
            "172.32.0.1",
            "[2001:db8::1]",
            "0.0.0.0",
            "::",
            "1.1.1.1",
            "169.254.169.254",
            "169.254.1.1",
            "fe80::1",
            "[::ffff:169.254.169.254]",
        ] {
            assert!(!is_private_host(h), "{h}");
        }
    }

    #[test]
    fn non_public_ips() {
        for ip in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "0.0.0.0", "::", "::1", "fe80::1", "fd00::1", "::ffff:169.254.1.1"] {
            assert!(is_non_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "::ffff:1.1.1.1"] {
            assert!(!is_non_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn handshake_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let srv = tokio::spawn(async move {
            let open = read_open(&mut server).await.unwrap();
            assert_eq!(open, TunnelOpen { host: "localhost".into(), port: 80 });
            reply(&mut server, Err(TunnelErrCode::Forbidden)).await.unwrap();
        });
        let err = open(&mut client, "localhost", 80).await.unwrap_err();
        let err = err.get_ref().and_then(|e| e.downcast_ref::<TunnelErr>()).copied();
        assert_eq!(err, Some(TunnelErr { code: TunnelErrCode::Forbidden }));
        srv.await.unwrap();
    }
}
