use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

use crate::net::RemoteConn;

/// Any bidirectional byte stream usable by silo framing.
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

/// Unified stream type for both unix sockets and iroh bi streams.
pub type Stream = Box<dyn AsyncStream>;

/// One QUIC bidirectional stream as a single read/write object.
/// `poll_shutdown` finishes the send half.
#[derive(Debug)]
pub struct BiStream {
    send: SendStream,
    recv: RecvStream,
}

impl BiStream {
    pub(crate) fn new(send: SendStream, recv: RecvStream) -> Self {
        BiStream { send, recv }
    }
}

impl AsyncRead for BiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for BiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.send)
            .poll_write(cx, buf)
            .map_err(io::Error::other)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

/// A client's route to a server: local unix socket or remote iroh connection.
#[derive(Debug, Clone)]
pub enum Conn {
    Unix(PathBuf),
    Iroh(RemoteConn),
}

impl Conn {
    /// Unix: connect a new socket. Iroh: open a new bi stream on the
    /// existing connection.
    pub async fn open_stream(&self) -> Result<Stream> {
        match self {
            Conn::Unix(path) => {
                let s = UnixStream::connect(path)
                    .await
                    .with_context(|| format!("connect {}", path.display()))?;
                Ok(Box::new(s))
            }
            Conn::Iroh(rc) => Ok(Box::new(rc.open_stream().await?)),
        }
    }
}
